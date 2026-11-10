//! One hit test after both panes render keeps nested targets and either drag
//! direction consistent. Filesystem validation and all I/O stay in the worker.
use crate::{fs_ops::TransferKind, vfs::Location};
use eframe::egui::{self, Color32, LayerId, Rect};

pub struct DragFiles(pub Vec<Location>);

pub struct DropRequest {
    pub sources: Vec<Location>,
    pub destination: Location,
    pub pane: usize,
    pub kind: TransferKind,
}

struct Target {
    rect: Rect,
    layer: LayerId,
    destination: Location,
    pane: usize,
    ready: bool,
}

#[derive(Default)]
pub struct DropTargets(Vec<Target>);

impl DropTargets {
    // Register broad backgrounds before their more specific child targets.
    pub fn add(
        &mut self,
        ui: &egui::Ui,
        rect: Rect,
        pane: usize,
        destination: &Location,
        ready: bool,
    ) {
        self.0.push(Target {
            rect: rect.intersect(ui.clip_rect()),
            layer: ui.layer_id(),
            destination: destination.clone(),
            pane,
            ready,
        });
    }

    fn at(&self, pos: egui::Pos2, layer: Option<LayerId>) -> Option<&Target> {
        self.0
            .iter()
            .rev()
            .find(|t| t.rect.contains(pos) && Some(t.layer) == layer)
    }

    pub fn finish(
        &self,
        ctx: &egui::Context,
        enabled: bool,
    ) -> Option<Result<DropRequest, String>> {
        if !enabled {
            egui::DragAndDrop::clear_payload(ctx);
            return None;
        }
        let internal = egui::DragAndDrop::payload::<DragFiles>(ctx);
        let (hovered, dropped, pos, released, shift) = ctx.input(|i| {
            (
                i.raw.hovered_files.clone(),
                i.raw.dropped_files.clone(),
                i.pointer.hover_pos(),
                i.pointer.button_released(egui::PointerButton::Primary),
                i.modifiers.shift,
            )
        });
        let external = !hovered.is_empty() || !dropped.is_empty();
        if !external && internal.is_none() {
            return None;
        }
        let committing = if external {
            !dropped.is_empty()
        } else {
            released
        };
        let sources = if external {
            if dropped.is_empty() {
                hovered
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .map(Location::Local)
                    .collect()
            } else {
                match dropped
                    .iter()
                    .map(|f| f.path.clone().map(Location::Local))
                    .collect::<Option<Vec<_>>>()
                {
                    Some(paths) => paths,
                    None => {
                        return Some(Err(crate::i18n::text(
                            "Перетащите файлы с диска: у части объектов нет локального пути",
                        )
                        .into()))
                    }
                }
            }
        } else {
            internal.as_ref()?.0.clone()
        };
        // Native drops only copy: the OS source has no move-completion protocol
        // with this app, so it must never lose its originals.
        let kind = if shift && !external {
            TransferKind::Move
        } else {
            TransferKind::Copy
        };
        let target = pos.and_then(|p| self.at(p, ctx.layer_id_at(p)));
        let error = match target {
            None => {
                Some(crate::i18n::text("Перетащите на папку или в область файлов панели").into())
            }
            Some(t) if !t.ready => {
                Some(crate::i18n::text("Дождитесь окончания чтения папки").into())
            }
            Some(t) => validate(&sources, &t.destination, kind).err(),
        };
        let label = if let Some(error) = &error {
            error.clone()
        } else {
            let t = target?;
            format!(
                "{}: {} → {}{}",
                if kind == TransferKind::Move {
                    crate::i18n::text("Переместить")
                } else {
                    crate::i18n::text("Копировать")
                },
                if sources.is_empty() {
                    hovered.len()
                } else {
                    sources.len()
                },
                t.destination.name(),
                if external {
                    ""
                } else {
                    crate::i18n::text(" · Shift: переместить · Esc: отмена")
                }
            )
        };
        if let Some(pos) = pos {
            let color = if error.is_some() {
                Color32::from_rgb(200, 70, 75)
            } else {
                ctx.style().visuals.selection.stroke.color
            };
            let painter = ctx.layer_painter(LayerId::new(
                egui::Order::Tooltip,
                egui::Id::new("file_drop_feedback"),
            ));
            if let Some(t) = target {
                painter.rect_stroke(
                    t.rect.shrink(1.0),
                    4,
                    egui::Stroke::new(2.0, color),
                    egui::StrokeKind::Inside,
                );
            }
            let galley = painter.layout(
                label,
                egui::FontId::proportional(12.0),
                ctx.style().visuals.text_color(),
                360.0,
            );
            let badge = Rect::from_min_size(
                pos + egui::vec2(16.0, 20.0),
                galley.size() + egui::vec2(16.0, 12.0),
            );
            let badge = badge.translate(egui::vec2(
                (ctx.screen_rect().right() - badge.right()).min(0.0),
                (ctx.screen_rect().bottom() - badge.bottom()).min(0.0),
            ));
            painter.rect_filled(badge, 5, ctx.style().visuals.extreme_bg_color);
            painter.rect_stroke(
                badge,
                5,
                egui::Stroke::new(1.0, color),
                egui::StrokeKind::Inside,
            );
            painter.galley(
                badge.min + egui::vec2(8.0, 6.0),
                galley,
                ctx.style().visuals.text_color(),
            );
        }
        if !committing {
            return None;
        }
        egui::DragAndDrop::clear_payload(ctx);
        // Releasing outside registered targets cancels an internal drag.
        if target.is_none() && !external {
            return None;
        }
        if let Some(error) = error {
            return Some(Err(error));
        }
        let target = target?;
        Some(Ok(DropRequest {
            sources,
            destination: target.destination.clone(),
            pane: target.pane,
            kind,
        }))
    }
}

fn validate(
    sources: &[Location],
    destination: &Location,
    kind: TransferKind,
) -> Result<(), String> {
    for source in sources {
        if kind == TransferKind::Move && !source.same_backend(destination) {
            return Err(crate::i18n::text(
                "Между подключениями используйте копирование: отпустите Shift",
            )
            .into());
        }
        if source.same_backend(destination) {
            if destination.path().starts_with(source.path()) {
                return Err(crate::i18n::text(
                    "Нельзя переносить элемент в себя или вложенную папку",
                )
                .into());
            }
            if source.parent().as_ref() == Some(destination) {
                return Err(crate::i18n::text("Элементы уже находятся в этой папке").into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, vec2, Event, Modifiers, PointerButton, RawInput};

    fn local(path: &str) -> Location {
        Location::Local(path.into())
    }
    fn remote(host: &str, path: &str) -> Location {
        Location::Sftp {
            endpoint: crate::vfs::Endpoint {
                host: host.into(),
                port: 22,
                user: "test".into(),
                key: None,
            },
            path: path.into(),
        }
    }
    fn input(events: Vec<Event>) -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 400.0))),
            events,
            ..Default::default()
        }
    }
    fn button(pos: egui::Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }
    fn frame(
        ctx: &egui::Context,
        input: RawInput,
        enabled: bool,
        ready: bool,
    ) -> Option<Result<DropRequest, String>> {
        let mut result = None;
        let _ = ctx.run(input, |ctx| {
            let mut targets = DropTargets::default();
            egui::CentralPanel::default().show(ctx, |ui| {
                targets.add(
                    ui,
                    Rect::from_min_max(pos2(10.0, 10.0), pos2(590.0, 390.0)),
                    0,
                    &local("/target"),
                    ready,
                );
                targets.add(
                    ui,
                    Rect::from_min_max(pos2(200.0, 100.0), pos2(300.0, 150.0)),
                    1,
                    &local("/target/nested"),
                    ready,
                );
            });
            result = targets.finish(ctx, enabled);
        });
        result
    }

    #[test]
    fn internal_drop_uses_nested_target_and_emits_once() {
        let ctx = egui::Context::default();
        let pos = pos2(220.0, 120.0);
        frame(
            &ctx,
            input(vec![Event::PointerMoved(pos), button(pos, true)]),
            true,
            true,
        );
        egui::DragAndDrop::set_payload(
            &ctx,
            DragFiles(vec![local("/source/a"), local("/source/b")]),
        );
        assert!(frame(&ctx, input(vec![]), true, true).is_none());
        let request = frame(&ctx, input(vec![button(pos, false)]), true, true)
            .unwrap()
            .unwrap();
        assert_eq!(request.destination, local("/target/nested"));
        assert_eq!(request.pane, 1);
        assert_eq!(request.sources.len(), 2);
        assert_eq!(request.kind, TransferKind::Copy);
        assert!(frame(&ctx, input(vec![]), true, true).is_none());
    }

    #[test]
    fn shift_moves_but_native_drops_always_copy() {
        let ctx = egui::Context::default();
        let pos = pos2(50.0, 200.0);
        frame(
            &ctx,
            input(vec![Event::PointerMoved(pos), button(pos, true)]),
            true,
            true,
        );
        egui::DragAndDrop::set_payload(&ctx, DragFiles(vec![local("/source/a")]));
        let mut release = input(vec![button(pos, false)]);
        release.modifiers = Modifiers::SHIFT;
        let request = frame(&ctx, release, true, true).unwrap().unwrap();
        assert_eq!(request.kind, TransferKind::Move);
        let mut native = input(vec![Event::PointerMoved(pos)]);
        native.modifiers = Modifiers::SHIFT;
        native.dropped_files.push(egui::DroppedFile {
            path: Some("/source/a".into()),
            ..Default::default()
        });
        let request = frame(&ctx, native, true, true).unwrap().unwrap();
        assert_eq!(request.kind, TransferKind::Copy);
        assert_eq!(request.destination, local("/target"));
    }

    #[test]
    fn escape_and_drop_outside_cancel() {
        let ctx = egui::Context::default();
        let pos = pos2(5.0, 5.0);
        frame(
            &ctx,
            input(vec![Event::PointerMoved(pos), button(pos, true)]),
            true,
            true,
        );
        egui::DragAndDrop::set_payload(&ctx, DragFiles(vec![local("/source/a")]));
        assert!(frame(&ctx, input(vec![button(pos, false)]), true, true).is_none());
        egui::DragAndDrop::set_payload(&ctx, DragFiles(vec![local("/source/a")]));
        let escape = Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        assert!(frame(&ctx, input(vec![escape]), true, true).is_none());
        assert!(!egui::DragAndDrop::has_any_payload(&ctx));
    }

    #[test]
    fn native_batch_is_rejected_if_any_path_is_missing_and_busy_drops_are_ignored() {
        let ctx = egui::Context::default();
        let mut native = input(vec![Event::PointerMoved(pos2(50.0, 200.0))]);
        native.dropped_files = vec![
            egui::DroppedFile {
                path: Some("/source/a".into()),
                ..Default::default()
            },
            egui::DroppedFile::default(),
        ];
        assert!(frame(&ctx, native.clone(), true, true).unwrap().is_err());
        assert!(frame(&ctx, native, false, true).is_none());
        let mut native = input(vec![]);
        native.dropped_files.push(egui::DroppedFile {
            path: Some("/source/a".into()),
            ..Default::default()
        });
        assert!(frame(&ctx, native, true, false).unwrap().is_err());
    }

    #[test]
    fn rejects_self_descendants_same_folder_and_cross_backend_moves() {
        for kind in [TransferKind::Copy, TransferKind::Move] {
            for dest in ["/source", "/source/folder", "/source/folder/child"] {
                assert!(validate(&[local("/source/folder")], &local(dest), kind).is_err());
            }
            assert!(validate(
                &[local("/source/folder")],
                &local("/source/folder-other"),
                kind
            )
            .is_ok());
        }
        let source = remote("one", "/source/file");
        assert!(validate(
            std::slice::from_ref(&source),
            &remote("one", "/target"),
            TransferKind::Move
        )
        .is_ok());
        for destination in [local("/target"), remote("two", "/target")] {
            assert!(validate(
                std::slice::from_ref(&source),
                &destination,
                TransferKind::Move
            )
            .is_err());
            assert!(validate(
                std::slice::from_ref(&source),
                &destination,
                TransferKind::Copy
            )
            .is_ok());
            assert!(validate(&[destination], &source, TransferKind::Move).is_err());
        }
    }

    #[test]
    fn hit_test_respects_clip_and_covering_layers() {
        let layer = LayerId::background();
        let targets = DropTargets(vec![Target {
            rect: Rect::from_min_max(pos2(10.0, 10.0), pos2(30.0, 30.0)),
            layer,
            destination: local("/target"),
            pane: 0,
            ready: true,
        }]);
        assert!(targets.at(pos2(20.0, 20.0), Some(layer)).is_some());
        assert!(targets.at(pos2(31.0, 20.0), Some(layer)).is_none());
        assert!(targets
            .at(
                pos2(20.0, 20.0),
                Some(LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("popup")
                ))
            )
            .is_none());
    }
}
