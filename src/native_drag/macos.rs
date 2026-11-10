use crate::{drag_drop::DragFiles, vfs::Location};
use eframe::egui;
use objc2::{
    declare_class, msg_send_id, mutability, rc::Retained, runtime::ProtocolObject, ClassType,
    DeclaredClass,
};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSEventType, NSView, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSURL,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    ffi::CString,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    ptr::NonNull,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    },
};

struct SourceState {
    // 0: running, 1: cancelled, 2: accepted by the destination.
    result: Arc<AtomicU8>,
    ctx: egui::Context,
    view: Retained<NSView>,
    position: Arc<Mutex<Option<egui::Pos2>>>,
}

impl SourceState {
    fn moved(&self, screen_point: NSPoint) {
        if let Some(window) = self.view.window() {
            let point = self
                .view
                .convertPoint_fromView(window.convertPointFromScreen(screen_point), None);
            let bounds = self.view.bounds();
            let scale = f64::from(self.ctx.screen_rect().width()) / bounds.size.width;
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

declare_class!(
    struct DragSource;

    // SAFETY: NSObject has no subclass requirements. AppKit only calls this
    // source on the main thread, and its lifetime is held by ActiveDrag.
    unsafe impl ClassType for DragSource {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "PawCommanderFileDragSource";
    }
    impl DeclaredClass for DragSource { type Ivars = SourceState; }
    unsafe impl NSObjectProtocol for DragSource {}
    unsafe impl NSDraggingSource for DragSource {
        #[method(draggingSession:sourceOperationMaskForDraggingContext:)]
        fn operation(&self, _: &NSDraggingSession, _: NSDraggingContext) -> NSDragOperation {
            // A receiver may copy or open/attach the files. No receiver gets
            // permission to move/delete the source through this drag.
            NSDragOperation::Copy
        }

        #[method(draggingSession:movedToPoint:)]
        fn moved(&self, _: &NSDraggingSession, point: NSPoint) {
            self.ivars().moved(point);
        }

        #[method(draggingSession:endedAtPoint:operation:)]
        fn ended(&self, _: &NSDraggingSession, point: NSPoint, operation: NSDragOperation) {
            self.ivars().moved(point);
            self.ivars().result.store(if operation.is_empty() { 1 } else { 2 }, Ordering::Release);
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
}

#[derive(Default)]
pub struct NativeDrag {
    active: Option<ActiveDrag>,
}

impl NativeDrag {
    pub fn active(&self) -> bool {
        self.active.is_some()
    }

    pub fn finish_input(&mut self, input: &mut egui::RawInput) -> Option<bool> {
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
        self.active = None;
        release_pointer(input, position, bounds);
        Some(result == 2)
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
                            .is_some_and(|p| !i.screen_rect().shrink(12.0).contains(p)))
            })
        {
            return None;
        }
        let payload = egui::DragAndDrop::take_payload::<DragFiles>(ctx)?;
        let paths = match local_paths(&payload.0) {
            Ok(paths) => paths,
            Err(error) => return Some(Err(error)),
        };
        Some(self.start(ctx, frame, &paths))
    }

    fn start(
        &mut self,
        ctx: &egui::Context,
        frame: &eframe::Frame,
        paths: &[PathBuf],
    ) -> Result<(), String> {
        let mtm = MainThreadMarker::new().ok_or(crate::i18n::text(
            "Системное перетаскивание требует главного потока",
        ))?;
        let handle = frame.window_handle().map_err(|e| {
            crate::tr_format!("Не удалось получить окно: {e}", "Could not get window: {e}")
        })?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return Err(crate::i18n::text("Окно не поддерживает AppKit").into());
        };
        // SAFETY: eframe's borrowed handle points to its live NSView; all AppKit
        // operations below run on the main thread while the frame is borrowed.
        let view = unsafe { &*handle.ns_view.cast::<NSView>().as_ptr() };
        let window = view.window().ok_or(crate::i18n::text("Окно уже закрыто"))?;
        let app = NSApplication::sharedApplication(mtm);
        let previous = app.currentEvent().ok_or(crate::i18n::text(
            "Не удалось получить событие перетаскивания",
        ))?;
        let result = Arc::new(AtomicU8::new(0));
        let position = Arc::new(Mutex::new(None));
        let source = mtm.alloc::<DragSource>().set_ivars(SourceState {
            result: result.clone(),
            ctx: ctx.clone(),
            // SAFETY: the borrowed live NSView is retained for the session.
            view: unsafe { Retained::retain(view as *const NSView as *mut NSView) }
                .ok_or(crate::i18n::text("Окно уже закрыто"))?,
            position: position.clone(),
        });
        // SAFETY: init initializes the allocated NSObject subclass; all other
        // arguments below are live typed AppKit objects with valid geometry.
        unsafe {
            let source: Retained<DragSource> = msg_send_id![super(source), init];
            if NSEvent::pressedMouseButtons() & 1 == 0 {
                return Err(
                    crate::i18n::text("Перетаскивание отменено: кнопка мыши уже отпущена").into(),
                );
            }
            let location = window.convertPointFromScreen(NSEvent::mouseLocation());
            let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                NSEventType::LeftMouseDragged, location, NSEvent::modifierFlags_class(), previous.timestamp(), window.windowNumber(), None, 0, 1, 1.0,
            ).ok_or(crate::i18n::text("Не удалось создать событие перетаскивания"))?;
            let point = view.convertPoint_fromView(location, None);
            let workspace = NSWorkspace::sharedWorkspace();
            let mut items = Vec::with_capacity(paths.len());
            for (index, path) in paths.iter().enumerate() {
                let url = file_url(path)?;
                let writer = ProtocolObject::from_ref(&*url);
                let item =
                    NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
                let file_path = url
                    .path()
                    .ok_or(crate::i18n::text("Не удалось прочитать путь файла"))?;
                let icon = workspace.iconForFile(&file_path);
                let offset = (index.min(4) * 4) as f64;
                item.setDraggingFrame_contents(
                    NSRect::new(
                        NSPoint::new(point.x + offset, point.y + offset),
                        NSSize::new(40.0, 40.0),
                    ),
                    Some(&icon),
                );
                items.push(item);
            }
            let session = view.beginDraggingSessionWithItems_event_source(
                &NSArray::from_id_slice(&items),
                &event,
                ProtocolObject::from_ref(&*source),
            );
            self.active = Some(ActiveDrag {
                _source: source,
                _session: session,
                result,
                position,
                bounds: ctx.screen_rect(),
            });
        }
        Ok(())
    }
}

fn release_pointer(input: &mut egui::RawInput, position: Option<egui::Pos2>, bounds: egui::Rect) {
    // AppKit owns mouse-up/Escape during a native session; winit may never
    // receive that mouse-up. Release egui's original press exactly once.
    input.events.push(egui::Event::PointerButton {
        pos: position.unwrap_or(egui::Pos2::ZERO),
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: input.modifiers,
    });
    if !position.is_some_and(|p| bounds.contains(p)) {
        input.events.push(egui::Event::PointerGone);
    }
}

fn local_paths(sources: &[Location]) -> Result<Vec<PathBuf>, String> {
    if sources.is_empty() {
        return Err(crate::i18n::text("Не выбраны файлы для перетаскивания").into());
    }
    sources.iter().map(|source| {
        let path = source.local().ok_or(crate::i18n::text("Для переноса SFTP-файлов во внешнее приложение сначала скопируйте их в локальную папку"))?;
        let path = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().map_err(|e| e.to_string())?.join(path) };
        std::fs::symlink_metadata(&path).map_err(|e| crate::tr_format!("Не удалось передать {}: {e}", "Could not transfer {}: {e}", path.display()))?;
        Ok(path)
    }).collect()
}

fn file_url(path: &std::path::Path) -> Result<Retained<NSURL>, String> {
    let bytes = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| crate::i18n::text("Недопустимый путь файла"))?;
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    // SAFETY: the NUL-terminated filesystem byte string remains live for the
    // call. NSURL copies it. lstat preserves directory symlinks as symlinks.
    Ok(unsafe {
        NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(
            NonNull::new(bytes.as_ptr().cast_mut()).unwrap(),
            metadata.is_dir(),
            None,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_app_kit::NSPasteboardWriting;
    use std::{ffi::CStr, os::unix::fs::symlink};

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
            let _ = ctx.run(
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
            release_pointer(&mut input, end, bounds);
            let _ = ctx.run(input, |_| {});
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
            let _ = ctx.run(
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
                    egui::DragAndDrop::set_payload(ctx, DragFiles(vec![path.clone().into()]));
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
            let data = unsafe {
                writer
                    .pasteboardPropertyListForType(objc2_foundation::ns_string!("public.file-url"))
            };
            assert!(
                data.is_some(),
                "receivers need file URLs, not plain text paths"
            );
        }
        let link_url = file_url(&link).unwrap();
        assert!(!unsafe { link_url.absoluteString() }
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
