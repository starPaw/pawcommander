use super::file_promises::{FilePromises, PromiseRequest};
use super::PromiseKind;
#[cfg(test)]
use crate::test_ui::ContextExt as _;
use crate::{drag_drop::DragFiles, macos_file_url, vfs::Location};
use eframe::egui;
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    AnyThread, DefinedClass, MainThreadOnly,
};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSEventMask, NSEventType, NSImage, NSImageNameFolder,
    NSImageNameMultipleDocuments, NSPasteboardWriting, NSView, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSURL,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    cell::RefCell,
    path::PathBuf,
    ptr::NonNull,
    rc::Rc,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    },
};

unsafe extern "C" {
    fn paw_drag_escape_listen() -> i32;
    fn paw_drag_escape_start(window: isize) -> *mut NSObject;
    fn paw_drag_escape_stop(guard: &NSObject);
}

pub fn listen_for_escape() -> i32 {
    // SAFETY: the helper enters its own event loop on its main thread.
    unsafe { paw_drag_escape_listen() }
}

struct EscapeListener(Retained<NSObject>);

impl EscapeListener {
    fn stop(&self) {
        // SAFETY: the object owns only this drag's private control pipe.
        unsafe { paw_drag_escape_stop(&self.0) };
    }
}

impl Drop for EscapeListener {
    fn drop(&mut self) {
        self.stop();
    }
}

struct SourceState {
    escape: EscapeListener,
    // 0: running, 1: cancelled, 2: accepted by the destination.
    result: Arc<AtomicU8>,
    ctx: egui::Context,
    view: Retained<NSView>,
    position: Arc<Mutex<Option<egui::Pos2>>>,
}

pub(super) const DRAG_CANCELLED: u8 = 1;

impl SourceState {
    fn moved(&self, screen_point: NSPoint) {
        if let Some(window) = self.view.window() {
            let point = self
                .view
                .convertPoint_fromView(window.convertPointFromScreen(screen_point), None);
            let bounds = self.view.bounds();
            let scale = f64::from(self.ctx.viewport_rect().width()) / bounds.size.width;
            let y = if self.view.isFlipped() {
                point.y - bounds.origin.y
            } else {
                bounds.origin.y + bounds.size.height - point.y
            };
            if let Ok(mut position) = self.position.lock() {
                *position = Some(egui::pos2(
                    ((point.x - bounds.origin.x) * scale) as f32,
                    (y * scale) as f32,
                ));
            }
            self.ctx.request_repaint();
        }
    }
}

define_class!(
    // SAFETY: NSObject has no subclass requirements. AppKit only calls this
    // source on the main thread, and its lifetime is held by ActiveDrag.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PawCommanderFileDragSource"]
    #[ivars = SourceState]
    struct DragSource;

    unsafe impl NSObjectProtocol for DragSource {}
    unsafe impl NSDraggingSource for DragSource {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn operation(&self, _: &NSDraggingSession, _: NSDraggingContext) -> NSDragOperation {
            // A receiver may copy or open/attach the files. No receiver gets
            // permission to move/delete the source through this drag.
            NSDragOperation::Copy
        }

        #[unsafe(method(draggingSession:movedToPoint:))]
        fn moved(&self, _: &NSDraggingSession, point: NSPoint) {
            self.ivars().moved(point);
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _: &NSDraggingSession, point: NSPoint, operation: NSDragOperation) {
            self.ivars().escape.stop();
            self.ivars().moved(point);
            self.ivars().result.store(
                if operation.is_empty() {
                    DRAG_CANCELLED
                } else {
                    2
                },
                Ordering::Release,
            );
            self.ivars().ctx.request_repaint();
        }
    }
);

struct ActiveDrag {
    _source: Retained<DragSource>,
    _session: Retained<NSDraggingSession>,
    result: Arc<AtomicU8>,
    position: Arc<Mutex<Option<egui::Pos2>>>,
    bounds: egui::Rect,
    promised: bool,
}

// A frame can be drawn after both mouseDown and mouseDragged. Capture the
// original press during event delivery, before winit coalesces redraws.
struct MousePressMonitor {
    token: Retained<AnyObject>,
    press: Rc<RefCell<Option<Retained<NSEvent>>>>,
}

impl MousePressMonitor {
    fn install(_: MainThreadMarker) -> Option<Self> {
        let press = Rc::new(RefCell::new(None));
        let slot = Rc::clone(&press);
        let handler = block2::RcBlock::new(move |event: NonNull<NSEvent>| {
            // SAFETY: AppKit supplies a live event, invokes local monitors on
            // the main thread, and expects this unmodified event back.
            let value = unsafe { event.as_ref() };
            *slot.borrow_mut() = if value.r#type() == NSEventType::LeftMouseDown {
                unsafe { Retained::retain(event.as_ptr()) }
            } else {
                None
            };
            event.as_ptr()
        });
        let token = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::LeftMouseDown | NSEventMask::LeftMouseUp,
                &handler,
            )
        }?;
        Some(Self { token, press })
    }
}

impl Drop for MousePressMonitor {
    fn drop(&mut self) {
        // SAFETY: this is the live token returned by our main-thread monitor.
        unsafe { NSEvent::removeMonitor(&self.token) };
    }
}

#[derive(Default)]
pub struct NativeDrag {
    active: Option<ActiveDrag>,
    mouse_monitor: Option<MousePressMonitor>,
    deferred_input: Vec<egui::Event>,
    promises: FilePromises,
}

impl NativeDrag {
    pub fn prepare_input(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        // egui hit-tests the final pointer position of a frame. If the press
        // and a fast movement arrive together, first let the row at the press
        // position claim the gesture, then deliver the movement in order.
        if !self.deferred_input.is_empty() {
            self.deferred_input.append(&mut input.events);
            input.events = std::mem::take(&mut self.deferred_input);
        }
        if !input.focused || self.active() {
            return;
        }
        if let Some(split) = fast_drag_split(&input.events) {
            self.deferred_input = input.events.split_off(split);
            ctx.request_repaint();
        }
    }

    pub fn active(&self) -> bool {
        self.active.is_some()
    }

    pub fn finish_input(&mut self, input: &mut egui::RawInput) -> Option<bool> {
        if self.mouse_monitor.is_none()
            && let Some(mtm) = MainThreadMarker::new()
        {
            self.mouse_monitor = MousePressMonitor::install(mtm);
        }
        let active = self.active.as_ref()?;
        let position = active.position.lock().ok().and_then(|p| *p);
        if let Some(pos) = position {
            input.events.push(egui::Event::PointerMoved(pos));
        }
        let result = active.result.load(Ordering::Acquire);
        if result == 0 {
            return None;
        }
        let bounds = active.bounds;
        let promised = active.promised;
        let modifiers = active._source.ivars().ctx.input(|input| input.modifiers);
        self.active = None;
        release_pointer(input, position, bounds, modifiers);
        Some(result == 2 && !promised)
    }

    pub(crate) fn next_promise(&self) -> Option<PromiseRequest> {
        self.promises.receive()
    }

    pub fn handoff(
        &mut self,
        ctx: &egui::Context,
        frame: &eframe::Frame,
        enabled: bool,
    ) -> Option<Result<(), String>> {
        if !enabled
            || self.active()
            || !ctx.input(|i| {
                i.pointer.primary_down()
                    && (i
                        .events
                        .iter()
                        .any(|e| matches!(e, egui::Event::PointerGone))
                        || i.pointer
                            .latest_pos()
                            .is_some_and(|p| !i.viewport_rect().shrink(12.0).contains(p)))
            })
        {
            return None;
        }
        let payload = egui::DragAndDrop::take_payload::<DragFiles>(ctx)?;
        let items = match drag_items(&payload) {
            Ok(items) => items,
            Err(error) => return Some(Err(error)),
        };
        Some(self.start(ctx, frame, &items))
    }

    fn start(
        &mut self,
        ctx: &egui::Context,
        frame: &eframe::Frame,
        entries: &[DragItem],
    ) -> Result<(), String> {
        let mtm = MainThreadMarker::new().ok_or(crate::i18n::text!(
            "Системное перетаскивание требует главного потока",
        ))?;
        let handle = frame
            .window_handle()
            .map_err(|e| crate::tr_format!("Could not get window: {e}", e = e))?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return Err(crate::i18n::text!("Окно не поддерживает AppKit").into());
        };
        // SAFETY: eframe's borrowed handle points to its live NSView; all AppKit
        // operations below run on the main thread while the frame is borrowed.
        let view = unsafe { &*handle.ns_view.cast::<NSView>().as_ptr() };
        let window = view
            .window()
            .ok_or(crate::i18n::text!("Окно уже закрыто"))?;
        let app = NSApplication::sharedApplication(mtm);
        let event = self
            .mouse_monitor
            .as_ref()
            .and_then(|monitor| monitor.press.borrow_mut().take())
            .or_else(|| app.currentEvent())
            .filter(|event| {
                drag_event_matches(event.r#type(), event.windowNumber(), window.windowNumber())
            })
            .ok_or(crate::i18n::text!(
                "Не удалось получить событие перетаскивания",
            ))?;
        let result = Arc::new(AtomicU8::new(0));
        let position = Arc::new(Mutex::new(None));
        // SAFETY: the bridge returns an owned token for this window or null.
        let token = unsafe { Retained::from_raw(paw_drag_escape_start(window.windowNumber())) }
            .ok_or(crate::i18n::text!(
                "Не удалось получить событие перетаскивания",
            ))?;
        let source = DragSource::alloc(mtm).set_ivars(SourceState {
            escape: EscapeListener(token),
            result: result.clone(),
            ctx: ctx.clone(),
            // SAFETY: the borrowed live NSView is retained for the session.
            view: unsafe { Retained::retain(view as *const NSView as *mut NSView) }
                .ok_or(crate::i18n::text!("Окно уже закрыто"))?,
            position: position.clone(),
        });
        // SAFETY: init initializes the allocated NSObject subclass; all other
        // arguments below are live typed AppKit objects with valid geometry.
        unsafe {
            let source: Retained<DragSource> = msg_send![super(source), init];
            if NSEvent::pressedMouseButtons() & 1 == 0 {
                return Err(crate::i18n::text!(
                    "Перетаскивание отменено: кнопка мыши уже отпущена"
                )
                .into());
            }
            let location = window.convertPointFromScreen(NSEvent::mouseLocation());
            let point = view.convertPoint_fromView(location, None);
            let workspace = NSWorkspace::sharedWorkspace();
            let mut items = Vec::with_capacity(entries.len());
            let mut promised = false;
            for (index, entry) in entries.iter().enumerate() {
                let (writer, icon): (Retained<ProtocolObject<dyn NSPasteboardWriting>>, _) =
                    match entry {
                        DragItem::Local(path) => {
                            let url = file_url(path)?;
                            let file_path = url
                                .path()
                                .ok_or(crate::i18n::text!("Не удалось прочитать путь файла"))?;
                            (
                                ProtocolObject::from_retained(url),
                                Some(workspace.iconForFile(&file_path)),
                            )
                        }
                        DragItem::Remote(path, kind) => {
                            promised = true;
                            let provider = self
                                .promises
                                .provider(path, *kind, ctx, &result)
                                .map_err(|error| crate::i18n::file_error(&error))?;
                            let icon = NSImage::imageNamed(match kind {
                                PromiseKind::Directory => NSImageNameFolder,
                                PromiseKind::File => NSImageNameMultipleDocuments,
                            });
                            (ProtocolObject::from_retained(provider), icon)
                        }
                    };
                let item =
                    NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), &writer);
                let offset = (index.min(4) * 4) as f64;
                item.setDraggingFrame_contents(
                    NSRect::new(
                        NSPoint::new(point.x + offset, point.y + offset),
                        NSSize::new(40.0, 40.0),
                    ),
                    icon.as_deref().map(|icon| icon.as_ref()),
                );
                items.push(item);
            }
            let session = view.beginDraggingSessionWithItems_event_source(
                &NSArray::from_retained_slice(&items),
                &event,
                ProtocolObject::from_ref(&*source),
            );
            self.active = Some(ActiveDrag {
                _source: source,
                _session: session,
                result,
                position,
                bounds: ctx.viewport_rect(),
                promised,
            });
        }
        Ok(())
    }
}

enum DragItem {
    Local(PathBuf),
    Remote(Location, PromiseKind),
}

fn drag_items(payload: &DragFiles) -> Result<Vec<DragItem>, String> {
    if payload.paths.iter().all(|path| !path.remote()) {
        return local_paths(&payload.paths)
            .map(|paths| paths.into_iter().map(DragItem::Local).collect());
    }
    if payload.promise_kinds.len() != payload.paths.len() {
        return Err(
            crate::i18n::text!("Не удалось подготовить SFTP-объекты для перетаскивания.").into(),
        );
    }
    payload
        .paths
        .iter()
        .zip(&payload.promise_kinds)
        .map(|(path, kind)| match kind {
            Some(kind) if path.remote() => Ok(DragItem::Remote(path.clone(), *kind)),
            _ => {
                Err(crate::i18n::text!("Из SFTP можно перетаскивать обычные файлы и папки.").into())
            }
        })
        .collect()
}

fn fast_drag_split(events: &[egui::Event]) -> Option<usize> {
    let mut press = None;
    for (index, event) in events.iter().enumerate() {
        match event {
            egui::Event::PointerButton {
                button: egui::PointerButton::Primary,
                pressed,
                pos,
                ..
            } => {
                press = pressed.then_some(*pos);
            }
            egui::Event::PointerMoved(pos)
                if press.is_some_and(|origin| pos.distance(origin) > 6.0) =>
            {
                return Some(index);
            }
            egui::Event::PointerGone if press.is_some() => return Some(index),
            _ => {}
        }
    }
    None
}

fn drag_event_matches(event_type: NSEventType, event_window: isize, source_window: isize) -> bool {
    event_type == NSEventType::LeftMouseDown && source_window > 0 && event_window == source_window
}

#[test]
fn drag_start_rejects_redraw_keyboard_and_other_window_events() {
    assert!(drag_event_matches(NSEventType::LeftMouseDown, 17, 17));
    for event_type in [
        NSEventType::LeftMouseDragged,
        NSEventType::ApplicationDefined,
        NSEventType::KeyDown,
        NSEventType::MouseMoved,
        NSEventType::LeftMouseUp,
    ] {
        assert!(!drag_event_matches(event_type, 17, 17));
    }
    assert!(!drag_event_matches(NSEventType::LeftMouseDown, 18, 17));
    assert!(!drag_event_matches(NSEventType::LeftMouseDown, 0, 0));
}

fn release_pointer(
    input: &mut egui::RawInput,
    position: Option<egui::Pos2>,
    bounds: egui::Rect,
    previous_modifiers: egui::Modifiers,
) {
    // AppKit owns mouse-up/Escape during a native session; winit may never
    // receive that mouse-up. Release egui's original press exactly once.
    let modifiers = input
        .events
        .iter()
        .rev()
        .find_map(|event| match event {
            egui::Event::ModifiersChanged(modifiers) => Some(*modifiers),
            _ => None,
        })
        .unwrap_or(previous_modifiers);
    input.events.push(egui::Event::PointerButton {
        pos: position.unwrap_or(egui::Pos2::ZERO),
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers,
    });
    if !position.is_some_and(|p| bounds.contains(p)) {
        input.events.push(egui::Event::PointerGone);
    }
}

fn local_paths(sources: &[Location]) -> Result<Vec<PathBuf>, String> {
    if sources.is_empty() {
        return Err(crate::i18n::text!("Не выбраны файлы для перетаскивания").into());
    }
    sources.iter().map(|source| {
        let path = source.local().ok_or(crate::i18n::text!("Для переноса SFTP-файлов во внешнее приложение сначала скопируйте их в локальную папку"))?;
        let path = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().map_err(|e| e.to_string())?.join(path) };
        std::fs::symlink_metadata(&path).map_err(|e| crate::tr_format!("Could not transfer {}: {e}", path.display(), e = e))?;
        Ok(path)
    }).collect()
}

fn file_url(path: &std::path::Path) -> Result<Retained<NSURL>, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    macos_file_url::from_path(path, metadata.is_dir())
        .map_err(|_| crate::i18n::text!("Недопустимый путь файла").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_app_kit::NSPasteboardWriting;
    use std::{
        ffi::CStr,
        os::unix::{ffi::OsStrExt, fs::symlink},
    };

    #[test]
    fn native_completion_preserves_latest_modifiers() {
        for (events, expected) in [
            (vec![], egui::Modifiers::SHIFT),
            (
                vec![
                    egui::Event::ModifiersChanged(egui::Modifiers::ALT),
                    egui::Event::ModifiersChanged(egui::Modifiers::CTRL),
                ],
                egui::Modifiers::CTRL,
            ),
        ] {
            let mut input = egui::RawInput {
                events,
                ..Default::default()
            };
            release_pointer(
                &mut input,
                Some(egui::Pos2::ZERO),
                egui::Rect::EVERYTHING,
                egui::Modifiers::SHIFT,
            );
            assert!(matches!(
                input.events.last(),
                Some(egui::Event::PointerButton { pressed: false, modifiers, .. })
                    if *modifiers == expected
            ));
        }
    }

    #[test]
    fn native_completion_releases_egui_pointer_inside_and_outside() {
        let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 500.0));
        for end in [
            Some(egui::pos2(300.0, 200.0)),
            Some(egui::pos2(900.0, 200.0)),
            None,
        ] {
            let ctx = egui::Context::default();
            let start = egui::pos2(50.0, 50.0);
            let _ = ctx.run_test_ui(
                egui::RawInput {
                    screen_rect: Some(bounds),
                    events: vec![
                        egui::Event::PointerMoved(start),
                        egui::Event::PointerButton {
                            pos: start,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |_| {},
            );
            assert!(ctx.input(|i| i.pointer.primary_down()));
            let mut input = egui::RawInput {
                screen_rect: Some(bounds),
                ..Default::default()
            };
            if let Some(pos) = end {
                input.events.push(egui::Event::PointerMoved(pos));
            }
            release_pointer(&mut input, end, bounds, egui::Modifiers::NONE);
            let _ = ctx.run_test_ui(input, |_| {});
            assert!(
                !ctx.input(|i| i.pointer.primary_down()),
                "native sessions must not leave a stuck drag"
            );
            if end.is_some_and(|p| bounds.contains(p)) {
                assert_eq!(ctx.input(|i| i.pointer.hover_pos()), end);
            } else {
                assert!(ctx.input(|i| i.pointer.hover_pos()).is_none());
            }
        }
    }

    #[test]
    fn native_handoff_waits_for_window_boundary_and_a_held_button() {
        let ctx = egui::Context::default();
        let mut native = NativeDrag::default();
        let frame = eframe::Frame::_new_kittest();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"data").unwrap();
        let mut at = |pos, pressed, enabled| {
            let mut result = None;
            let _ = ctx.run_test_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 500.0),
                    )),
                    events: vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |ctx| {
                    egui::DragAndDrop::set_payload(
                        ctx,
                        DragFiles::test_files(vec![path.clone().into()]),
                    );
                    result = native.handoff(ctx, &frame, enabled);
                },
            );
            result
        };
        assert!(at(egui::pos2(400.0, 200.0), true, true).is_none());
        assert!(at(egui::pos2(799.0, 200.0), true, false).is_none());
        assert!(at(egui::pos2(799.0, 200.0), false, true).is_none());
        // A test Frame has no native window; reaching that validation is proof
        // the boundary handoff was attempted, without starting an OS drag.
        assert!(at(egui::pos2(799.0, 200.0), true, true).unwrap().is_err());
        assert!(!egui::DragAndDrop::has_any_payload(&ctx));
    }

    #[test]
    fn native_urls_preserve_unicode_spaces_multiple_files_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("Привет #100% two words.txt");
        let directory = root.path().join("Folder");
        let link = root.path().join("link to folder");
        std::fs::write(&file, b"native drag").unwrap();
        std::fs::create_dir(&directory).unwrap();
        symlink(&directory, &link).unwrap();
        let sources = vec![
            Location::Local(file.clone()),
            Location::Local(directory.clone()),
            Location::Local(link.clone()),
        ];
        let paths = local_paths(&sources).unwrap();
        assert_eq!(paths, vec![file.clone(), directory.clone(), link.clone()]);
        for path in &paths {
            let url = file_url(path).unwrap();
            // SAFETY: the returned string belongs to the live NSURL.
            let bytes =
                unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) }.to_bytes();
            assert_eq!(bytes, path.as_os_str().as_bytes());
            let writer: &ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting> =
                ProtocolObject::from_ref(&*url);
            let data = writer
                .pasteboardPropertyListForType(objc2_foundation::ns_string!("public.file-url"));
            assert!(
                data.is_some(),
                "receivers need file URLs, not plain text paths"
            );
        }
        let link_url = file_url(&link).unwrap();
        assert!(!link_url
            .absoluteString()
            .unwrap()
            .to_string()
            .ends_with('/'));
        assert_eq!(std::fs::read(&file).unwrap(), b"native drag");
        assert_eq!(std::fs::read_link(&link).unwrap(), directory);
    }

    #[test]
    fn rejects_missing_files_and_remote_batches_before_native_session() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("exists");
        std::fs::write(&path, b"keep").unwrap();
        assert!(local_paths(&[]).is_err());
        assert!(local_paths(&[path.clone().into(), root.path().join("missing").into()]).is_err());
        let remote = Location::Sftp {
            endpoint: crate::vfs::Endpoint {
                host: "server".into(),
                port: 22,
                user: "user".into(),
                key: None,
                jump: None,
            },
            path: "/remote".into(),
        };
        assert!(local_paths(&[path.clone().into(), remote]).is_err());
        let broken = root.path().join("broken");
        symlink("missing", &broken).unwrap();
        assert_eq!(local_paths(&[broken.clone().into()]).unwrap(), vec![broken]);
        assert_eq!(std::fs::read(path).unwrap(), b"keep");
    }
}
