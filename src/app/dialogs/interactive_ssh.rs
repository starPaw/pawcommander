use crate::{
    i18n::text,
    theme::paint_focus,
    vfs::{AuthRequest, ConnectRequest, ConnectedLocation, ConnectionControl, InteractiveAuth},
};
use eframe::egui;
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

pub(in crate::app) struct ConnectionJob {
    pub pane: usize,
    pub result: mpsc::Receiver<Result<ConnectedLocation, String>>,
    target: String,
    control: ConnectionControl,
    requests: mpsc::Receiver<AuthRequest>,
    prompt: Option<PromptForm>,
    round: u64,
}

struct PromptForm {
    request: AuthRequest,
    answers: Vec<Zeroizing<String>>,
    focus_first: bool,
}

impl ConnectionJob {
    pub(super) fn launch(
        pane: usize,
        request: ConnectRequest,
        ctx: &egui::Context,
    ) -> Result<Self, String> {
        let target = format!(
            "{}@{} · {}",
            request.endpoint.user, request.endpoint.host, request.endpoint.port
        );
        let repaint = ctx.clone();
        let (auth, requests) = InteractiveAuth::new(move || repaint.request_repaint());
        let control = auth.control();
        let (send, result) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("pawcommander-sftp-connect".into())
            .spawn(move || {
                let _ = send.send(crate::vfs::connect_location(request, auth));
                ctx.request_repaint();
            })
            .map_err(|error| {
                crate::tr_format!(
                    "Не удалось запустить подключение SFTP ({error})",
                    "Could not start SFTP connection ({error})"
                )
            })?;
        Ok(Self {
            pane,
            result,
            target,
            control,
            requests,
            prompt: None,
            round: 0,
        })
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) {
        while let Ok(request) = self.requests.try_recv() {
            if !self.control.can_cancel() {
                continue;
            }
            self.round = self.round.wrapping_add(1);
            let answers = request
                .questions
                .iter()
                .map(|_| Zeroizing::new(String::new()))
                .collect();
            self.prompt = Some(PromptForm {
                request,
                answers,
                focus_first: true,
            });
        }
        let can_cancel = self.control.can_cancel();
        let mut submit = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::unique("sftp_authentication")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 32.0).clamp(180.0, 470.0));
            ui.heading(text("Подключение SFTP"));
            ui.label(&self.target);
            let mut waiting_for_answer = false;
            let mut allow_answer = false;
            if let Some(form) = &mut self.prompt {
                waiting_for_answer = !form.answers.is_empty();
                let remaining = form
                    .request
                    .deadline
                    .saturating_duration_since(Instant::now());
                allow_answer = waiting_for_answer && can_cancel && !remaining.is_zero();
                egui::ScrollArea::vertical()
                    .max_height((ctx.content_rect().height() - 190.0).clamp(65.0, 360.0))
                    .show(ui, |ui| {
                        if !form.request.title.is_empty() {
                            ui.heading(&form.request.title);
                        }
                        if !form.request.instructions.is_empty() {
                            ui.label(&form.request.instructions);
                        }
                        ui.add_enabled_ui(allow_answer, |ui| {
                            for (index, (question, answer)) in form
                                .request
                                .questions
                                .iter()
                                .zip(&mut form.answers)
                                .enumerate()
                            {
                                ui.push_id((self.round, index), |ui| {
                                    let label = ui.label(&question.label);
                                    let edit = answer_field(ui, answer, question.echo)
                                        .labelled_by(label.id);
                                    if index == 0 && form.focus_first {
                                        edit.request_focus();
                                    }
                                });
                            }
                        });
                        form.focus_first = false;
                    });
                if allow_answer {
                    let seconds = remaining.as_secs_f64().ceil() as u64;
                    ui.label(crate::tr_format!(
                        "На ответ {seconds} с",
                        "{seconds} s to respond"
                    ));
                    ctx.request_repaint_after(Duration::from_secs(1));
                }
            }
            if !waiting_for_answer {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(text("Подключение…"));
                });
            }
            ui.horizontal(|ui| {
                let response = ui.add_enabled(can_cancel, egui::Button::new(text("Отмена")));
                paint_focus(ui, &response, 5);
                cancel = response.clicked();
                if waiting_for_answer {
                    let response =
                        ui.add_enabled(allow_answer, egui::Button::new(text("Отправить ответы")));
                    paint_focus(ui, &response, 5);
                    submit = response.clicked();
                }
            });
        });
        if cancel || ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            if self.control.cancel() {
                self.prompt = None;
                ctx.request_repaint();
            }
        } else if submit && let Some(form) = self.prompt.take() {
            form.request.answer(form.answers);
            ctx.request_repaint();
        }
    }
}

impl Drop for ConnectionJob {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

pub(super) fn answer_field(ui: &mut egui::Ui, answer: &mut String, echo: bool) -> egui::Response {
    let mut output = egui::TextEdit::singleline(answer).password(!echo).show(ui);
    output.state.clear_undoer();
    output.response.response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ui::ContextExt as _;
    use ssh2::{KeyboardInteractivePrompt, Prompt};

    #[test]
    fn ssh_interactive_dialog_masks_answers_and_cancels_the_next_round() {
        let ctx = egui::Context::default();
        let (mut auth, requests) = InteractiveAuth::new(|| {});
        let control = auth.control();
        let (_send, result) = mpsc::channel();
        let mut job = ConnectionJob {
            pane: 0,
            result,
            target: "account@fixture · 2222".into(),
            control,
            requests,
            prompt: None,
            round: 0,
        };
        let (done, observed) = mpsc::channel();
        let secret = "fixture-hidden-answer";
        let worker = std::thread::spawn(move || {
            let first = auth.prompt(
                "Sign-in",
                "First line\nSecond line",
                &[Prompt {
                    text: "One-time code".into(),
                    echo: false,
                }],
            );
            let second = auth.prompt(
                "Confirm",
                "Another step",
                &[Prompt {
                    text: "Confirmation".into(),
                    echo: false,
                }],
            );
            let _ = done.send((first == [secret], second.is_empty()));
        });
        let draw = |job: &mut ConnectionJob, events| {
            ctx.run_test_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 640.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| job.show(ui.ctx()),
            )
            .shapes
            .into_iter()
            .filter_map(|shape| match shape.shape {
                egui::Shape::Text(label) => Some((
                    label.galley.text().to_owned(),
                    label.pos + label.galley.size() * 0.5,
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
        };
        let wait = |job: &mut ConnectionJob, wanted: &str| {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let labels = draw(job, vec![]);
                if labels.iter().any(|(label, _)| label == wanted) {
                    return labels;
                }
                assert!(
                    Instant::now() < deadline,
                    "Missing authentication prompt {wanted}"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        let first = wait(&mut job, "One-time code");
        assert!(first
            .iter()
            .any(|(label, _)| label.contains("First line\nSecond line")));
        draw(&mut job, vec![]);
        let typed = draw(&mut job, vec![egui::Event::Text(secret.into())]);
        assert!(!typed.iter().any(|(label, _)| label.contains(secret)));
        assert!(job.prompt.as_ref().unwrap().answers[0].as_str() == secret);
        let pos = typed
            .iter()
            .find(|(label, _)| label == "Send answers")
            .unwrap()
            .1;
        for pressed in [true, false] {
            draw(
                &mut job,
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
        wait(&mut job, "Confirmation");
        assert!(job.prompt.as_ref().unwrap().answers[0].is_empty());
        draw(
            &mut job,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(job.prompt.is_none());
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(3)).unwrap(),
            (true, true)
        );
        worker.join().unwrap();
    }
}
