#[cfg(test)]
use crate::test_ui::ContextExt as _;
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
    pub kind: crate::i18n::MessageKey,
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
    focus_cancel: bool,
}

impl ConflictDialog {
    pub fn new(conflict: Conflict, reply: mpsc::Sender<Answer>) -> Self {
        Self {
            conflict,
            reply,
            apply_to_group: false,
            focus_cancel: true,
        }
    }

    /// Returns true once the worker has been given a decision.
    pub fn show(&mut self, ctx: &egui::Context) -> bool {
        // A key used to trigger the transfer or answer a previous prompt must
        // not act on a newly opened prompt. Require a fresh press to activate.
        ctx.input_mut(|input| {
            input.events.retain(|event| {
                !matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::Enter | egui::Key::Space,
                        pressed: true,
                        repeat,
                        ..
                    } if self.focus_cancel || *repeat
                )
            });
        });
        let conflict = &self.conflict;
        let mut decision = None;
        egui::Modal::new(egui::Id::unique("transfer_conflict")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 32.0).clamp(1.0, 520.0));
            ui.heading(crate::i18n::text!("Совпадение имён"));
            ui.label(crate::tr_format!(
                "Conflict {} of {}",
                conflict.index,
                conflict.count
            ));
            ui.add_space(8.0);
            entry(ui, crate::i18n::text!("Исходный объект"), &conflict.source);
            ui.separator();
            entry(
                ui,
                crate::i18n::text!("Существующий объект"),
                &conflict.target,
            );
            ui.add_space(12.0);
            if !conflict.can_replace {
                ui.label(crate::i18n::text!(
                    "Выберите свободное имя или пропустите этот объект.",
                ));
            }
            if conflict.index < conflict.count {
                ui.checkbox(
                    &mut self.apply_to_group,
                    crate::i18n::text!("Применить к остальным совпадениям"),
                );
            }
            ui.horizontal_wrapped(|ui| {
                let cancel = ui.button(crate::i18n::text!("Отменить операцию"));
                if self.focus_cancel && ui.is_visible() {
                    cancel.request_focus();
                    self.focus_cancel = false;
                }
                if cancel.clicked() {
                    decision = Some(Decision::Cancel);
                }
                if ui.button(crate::i18n::text!("Пропустить")).clicked() {
                    decision = Some(Decision::Skip);
                }
                if ui
                    .button(crate::i18n::text!("Сохранить обе версии"))
                    .clicked()
                {
                    decision = Some(Decision::KeepBoth);
                }
                if ui
                    .add_enabled(
                        conflict.can_replace,
                        egui::Button::new(crate::i18n::text!("Заменить")),
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
                apply_to_group: self.apply_to_group && decision != Decision::Cancel,
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
        |bytes| crate::tr_format!("{bytes} B", bytes = crate::i18n::display::integer(bytes)),
    );
    let date = entry.modified.map_or_else(
        || "—".to_owned(),
        |modified| {
            crate::i18n::display::date(
                chrono::DateTime::<chrono::Local>::from(modified),
                crate::i18n::display::DateStyle::Seconds,
            )
        },
    );
    ui.label(crate::tr_format!(
        "{} · Size {size} · Modified {date}",
        crate::i18n::text(entry.kind),
        date = date,
        size = size
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;

    fn frame(
        dialog: &mut ConflictDialog,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(420.0, 400.0),
                )),
                events,
                ..Default::default()
            },
            |root_ui| {
                let ctx = &root_ui.ctx().clone();
                dialog.show(ctx);
            },
        )
    }

    fn dialog(can_replace: bool, index: usize) -> (ConflictDialog, mpsc::Receiver<Answer>) {
        let entry = |path: &str, size| EntryDetails {
            location: PathBuf::from(path).into(),
            kind: crate::i18n::key!("Файл"),
            size: Some(size),
            modified: Some(UNIX_EPOCH),
        };
        let (tx, rx) = mpsc::channel();
        (
            ConflictDialog::new(
                Conflict {
                    source: entry("/source/file.txt", 17),
                    target: entry("/target/file.txt", 32),
                    can_replace,
                    index,
                    count: 3,
                },
                tx,
            ),
            rx,
        )
    }

    #[test]
    fn conflict_dialog_shows_metadata_and_returns_group_decision_or_cancel() {
        fn label(frame: &egui::FullOutput, text: &str) -> egui::Pos2 {
            frame
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(shape) = &shape.shape
                        && shape.galley.text().contains(text)
                    {
                        return Some(shape.pos + shape.galley.size() / 2.0);
                    }
                    None
                })
                .unwrap_or_else(|| panic!("missing text: {text}"))
        }

        let ctx = egui::Context::default();
        let (mut first, rx) = dialog(false, 1);
        frame(&mut first, &ctx, vec![]);
        let output = frame(&mut first, &ctx, vec![]);
        label(&output, "File · Size 17 B · Modified");
        label(&output, "File · Size 32 B · Modified");
        let mut click = |text| {
            let pos = label(&output, text);
            for pressed in [true, false] {
                frame(
                    &mut first,
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

        let (mut second, rx) = dialog(false, 2);
        frame(
            &mut second,
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

    #[test]
    fn each_conflict_starts_on_cancel_and_ignores_held_activation_keys() {
        let ctx = egui::Context::default();
        let key = |key, pressed| egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let (mut first, rx) = dialog(true, 1);
        frame(&mut first, &ctx, vec![]);
        frame(&mut first, &ctx, vec![]);
        frame(&mut first, &ctx, vec![key(egui::Key::Tab, true)]);
        frame(&mut first, &ctx, vec![key(egui::Key::Enter, true)]);
        assert_eq!(rx.try_recv().unwrap().decision, Decision::Skip);

        let (mut second, rx) = dialog(true, 2);
        for _ in 0..2 {
            frame(&mut second, &ctx, vec![key(egui::Key::Enter, true)]);
            assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        }
        frame(&mut second, &ctx, vec![key(egui::Key::Enter, false)]);
        frame(&mut second, &ctx, vec![key(egui::Key::Enter, true)]);
        assert_eq!(rx.try_recv().unwrap().decision, Decision::Cancel);
    }
}
