use crate::{drag_drop::DragFiles, vfs::Location};
use eframe::egui;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
use super::linux::Platform;
#[cfg(target_os = "windows")]
use super::windows::Platform;

#[derive(Default)]
pub struct NativeDrag {
    platform: Platform,
    modifiers: Option<egui::Modifiers>,
    completed: Option<Result<bool, String>>,
    error: Option<String>,
}

impl NativeDrag {
    pub fn active(&self) -> bool {
        self.modifiers.is_some()
    }

    pub fn finish_input(&mut self, input: &mut egui::RawInput) -> Option<bool> {
        if self.active() {
            // Returning to our own window must not reinterpret the same native
            // session as an unrelated incoming drop and submit a second job.
            input.dropped_files.clear();
            input.hovered_files.clear();
        }
        let result = self.completed.take().or_else(|| self.platform.poll())?;
        let previous = self.modifiers.take()?;
        release_pointer(input, previous);
        match result {
            Ok(accepted) => Some(accepted),
            Err(error) => {
                self.error = Some(error);
                Some(false)
            }
        }
    }

    pub fn handoff(
        &mut self,
        ctx: &egui::Context,
        frame: &eframe::Frame,
        enabled: bool,
    ) -> Option<Result<(), String>> {
        // Wayland must observe the real button serial on the window's existing
        // connection before a drag starts, not open a new connection at exit.
        self.platform.prepare(ctx, frame);
        if let Some(error) = self.error.take() {
            return Some(Err(error));
        }
        if self.active() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
            return None;
        }
        if !enabled || !at_boundary(ctx) {
            return None;
        }
        let payload = egui::DragAndDrop::take_payload::<DragFiles>(ctx)?;
        self.modifiers = Some(ctx.input(|i| i.modifiers));
        let result =
            local_paths(&payload.paths).and_then(|paths| self.platform.start(ctx, frame, paths));
        if result.is_err() {
            self.completed = Some(Ok(false));
        }
        ctx.request_repaint();
        Some(result)
    }
}

fn at_boundary(ctx: &egui::Context) -> bool {
    ctx.input(|i| {
        i.pointer.primary_down()
            && (i
                .events
                .iter()
                .any(|e| matches!(e, egui::Event::PointerGone))
                || i.pointer
                    .latest_pos()
                    .is_some_and(|p| !i.viewport_rect().shrink(12.0).contains(p)))
    })
}

fn local_paths(sources: &[Location]) -> Result<Vec<PathBuf>, String> {
    if sources.is_empty() {
        return Err(crate::i18n::text!("Не выбраны файлы для перетаскивания").into());
    }
    sources.iter().map(|source| {
        let path = source.local().ok_or(crate::i18n::text!("Для переноса SFTP-файлов во внешнее приложение сначала скопируйте их в локальную папку"))?;
        // No filesystem reads here: the receiver opens the exported paths.
        // In particular, do not resolve symlinks or touch network files on the UI thread.
        let path = if path.is_absolute() { path.to_owned() } else {
            std::env::current_dir().map_err(|e| e.to_string())?.join(path)
        };
        if path.as_os_str().as_encoded_bytes().contains(&0) {
            return Err(crate::i18n::text!("Недопустимый путь файла").into());
        }
        Ok(path)
    }).collect()
}

fn release_pointer(input: &mut egui::RawInput, previous: egui::Modifiers) {
    let modifiers = input
        .events
        .iter()
        .rev()
        .find_map(|event| match event {
            egui::Event::ModifiersChanged(modifiers) => Some(*modifiers),
            _ => None,
        })
        .unwrap_or(previous);
    input.events.push(egui::Event::PointerButton {
        pos: egui::Pos2::ZERO,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers,
    });
    input.events.push(egui::Event::PointerGone);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ui::ContextExt as _;

    #[derive(Debug)]
    struct Dropped(PathBuf);
    impl egui::DroppedFile for Dropped {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
        fn bytes(&self) -> Result<Vec<u8>, String> {
            Err("Test drop has no contents".into())
        }
    }

    #[test]
    fn completion_releases_button_and_keeps_latest_modifiers() {
        let ctx = egui::Context::default();
        let _ = ctx.run_test_ui(
            egui::RawInput {
                events: vec![egui::Event::PointerButton {
                    pos: egui::pos2(20.0, 30.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |_| {},
        );
        assert!(ctx.input(|i| i.pointer.primary_down()));
        let mut input = egui::RawInput {
            events: vec![egui::Event::ModifiersChanged(egui::Modifiers::CTRL)],
            ..Default::default()
        };
        release_pointer(&mut input, egui::Modifiers::SHIFT);
        assert!(
            matches!(input.events[1], egui::Event::PointerButton { pressed: false, modifiers, .. } if modifiers == egui::Modifiers::CTRL)
        );
        let _ = ctx.run_test_ui(input, |_| {});
        assert!(!ctx.input(|i| i.pointer.primary_down()));
        assert!(ctx.input(|i| i.pointer.hover_pos()).is_none());
    }

    #[test]
    fn handoff_preserves_internal_drags_until_held_at_boundary() {
        let ctx = egui::Context::default();
        let mut native = NativeDrag::default();
        let frame = eframe::Frame::_new_kittest();
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
                        DragFiles::test_files(vec![PathBuf::from("test-file").into()]),
                    );
                    result = native.handoff(ctx, &frame, enabled);
                },
            );
            result
        };
        assert!(at(egui::pos2(400.0, 200.0), true, true).is_none());
        assert!(at(egui::pos2(799.0, 200.0), true, false).is_none());
        assert!(at(egui::pos2(799.0, 200.0), false, true).is_none());
        assert!(at(egui::pos2(799.0, 200.0), true, true).unwrap().is_err());
        assert!(!egui::DragAndDrop::has_any_payload(&ctx));
        let mut input = egui::RawInput {
            dropped_files: vec![std::sync::Arc::new(Dropped(PathBuf::from("test-file")))],
            ..Default::default()
        };
        assert_eq!(native.finish_input(&mut input), Some(false));
        assert!(input.dropped_files.is_empty());
        assert_eq!(native.finish_input(&mut input), None);
    }
}
