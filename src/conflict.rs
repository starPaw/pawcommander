use crate::vfs::Location;
use eframe::egui;
use std::sync::mpsc;
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Replace,
    Skip,
    KeepBoth,
    Cancel,
}

pub struct EntryDetails {
    pub location: Location,
    pub kind: &'static str,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
}

pub struct Conflict {
    pub source: EntryDetails,
    pub target: EntryDetails,
    pub can_replace: bool,
    pub index: usize,
    pub count: usize,
}

pub struct Answer {
    pub decision: Decision,
    pub apply_to_group: bool,
}

pub struct ConflictDialog {
    conflict: Conflict,
    reply: mpsc::Sender<Answer>,
    apply_to_group: bool,
}

impl ConflictDialog {
    pub fn new(conflict: Conflict, reply: mpsc::Sender<Answer>) -> Self {
        Self {
            conflict,
            reply,
            apply_to_group: false,
        }
    }

    /// Returns true once the worker has been given a decision.
    pub fn show(&mut self, ctx: &egui::Context) -> bool {
        let conflict = &self.conflict;
        let mut decision = None;
        egui::Modal::new(egui::Id::new("transfer_conflict")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading(crate::i18n::text("Совпадение имён"));
            ui.label(crate::tr_format!(
                "Совпадение {} из {}",
                "Conflict {} of {}",
                conflict.index,
                conflict.count
            ));
            ui.add_space(8.0);
            entry(ui, crate::i18n::text("Исходный объект"), &conflict.source);
            ui.separator();
            entry(
                ui,
                crate::i18n::text("Существующий объект"),
                &conflict.target,
            );
            ui.add_space(12.0);
            if !conflict.can_replace {
                ui.label(crate::i18n::text(
                    "Замена доступна только для двух разных обычных файлов.",
                ));
            }
            if conflict.index < conflict.count {
                ui.checkbox(
                    &mut self.apply_to_group,
                    crate::i18n::text("Применить к остальным совпадениям"),
                );
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button(crate::i18n::text("Отменить операцию")).clicked() {
                    decision = Some(Decision::Cancel);
                }
                if ui.button(crate::i18n::text("Пропустить")).clicked() {
                    decision = Some(Decision::Skip);
                }
                if ui
                    .button(crate::i18n::text("Сохранить обе версии"))
                    .clicked()
                {
                    decision = Some(Decision::KeepBoth);
                }
                if ui
                    .add_enabled(
                        conflict.can_replace,
                        egui::Button::new(crate::i18n::text("Заменить")),
                    )
                    .clicked()
                {
                    decision = Some(Decision::Replace);
                }
            });
        });
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            decision = Some(Decision::Cancel);
        }
        if let Some(decision) = decision {
            let _ = self.reply.send(Answer {
                decision,
                apply_to_group: self.apply_to_group,
            });
            return true;
        }
        false
    }
}

fn entry(ui: &mut egui::Ui, title: &str, entry: &EntryDetails) {
    ui.strong(title);
    ui.add(egui::Label::new(entry.location.display()).truncate())
        .on_hover_text(entry.location.display());
    let size = entry.size.map_or_else(
        || "—".to_owned(),
        |bytes| crate::tr_format!("{bytes} Б", "{bytes} B"),
    );
    let date = entry.modified.map_or_else(
        || "—".to_owned(),
        |modified| {
            chrono::DateTime::<chrono::Local>::from(modified)
                .format("%d.%m.%Y %H:%M:%S")
                .to_string()
        },
    );
    ui.label(crate::tr_format!(
        "{} · Размер {size} · Изменён {date}",
        "{} · Size {size} · Modified {date}",
        entry.kind
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;

    #[test]
    fn conflict_dialog_shows_metadata_and_returns_group_decision_or_cancel() {
        fn frame(
            dialog: &mut ConflictDialog,
            ctx: &egui::Context,
            events: Vec<egui::Event>,
        ) -> egui::FullOutput {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(960.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    dialog.show(ctx);
                },
            )
        }

        fn label(frame: &egui::FullOutput, text: &str) -> egui::Pos2 {
            frame
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(shape) = &shape.shape {
                        if shape.galley.text().contains(text) {
                            return Some(shape.pos + shape.galley.size() / 2.0);
                        }
                    }
                    None
                })
                .unwrap_or_else(|| panic!("missing text: {text}"))
        }

        let create = || {
            let entry = |path: &str, size| EntryDetails {
                location: PathBuf::from(path).into(),
                kind: "Файл",
                size: Some(size),
                modified: Some(UNIX_EPOCH),
            };
            let (tx, rx) = mpsc::channel();
            (
                ConflictDialog::new(
                    Conflict {
                        source: entry("/source/file.txt", 17),
                        target: entry("/target/file.txt", 32),
                        can_replace: false,
                        index: 1,
                        count: 3,
                    },
                    tx,
                ),
                rx,
            )
        };
        let ctx = egui::Context::default();
        let (mut dialog, rx) = create();
        frame(&mut dialog, &ctx, vec![]);
        let output = frame(&mut dialog, &ctx, vec![]);
        label(&output, "Size 17 B · Modified");
        label(&output, "Size 32 B · Modified");
        let mut click = |text| {
            let pos = label(&output, text);
            for pressed in [true, false] {
                frame(
                    &mut dialog,
                    &ctx,
                    vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        };
        click("Replace");
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        click("Apply to remaining conflicts");
        click("Keep both");
        let answer = rx.try_recv().unwrap();
        assert_eq!(answer.decision, Decision::KeepBoth);
        assert!(answer.apply_to_group);

        let (mut dialog, rx) = create();
        frame(
            &mut dialog,
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: Some(egui::Key::Escape),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert_eq!(rx.try_recv().unwrap().decision, Decision::Cancel);
    }
}
