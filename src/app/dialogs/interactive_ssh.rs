use crate::{
    i18n::text,
    operations::TransferError,
    theme::paint_focus,
    vfs::{
        AuthRequest, ConnectRequest, ConnectedLocation, ConnectionControl, ConnectionPrompt,
        HostKeyRequest, InteractiveAuth,
    },
};
use eframe::egui;
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

pub(in crate::app) struct ConnectionJob {
    pub pane: usize,
    pub result: mpsc::Receiver<Result<ConnectedLocation, TransferError>>,
    target: String,
    control: ConnectionControl,
    requests: mpsc::Receiver<ConnectionPrompt>,
    prompt: Option<PendingPrompt>,
    round: u64,
}

enum PendingPrompt {
    HostKey(HostKeyRequest),
    Authentication(PromptForm),
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
        let target = request.endpoint.label();
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
                crate::tr_format!("Could not start SFTP connection ({error})", error = error)
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
            self.prompt = Some(match request {
                ConnectionPrompt::HostKey(request) => PendingPrompt::HostKey(request),
                ConnectionPrompt::Authentication(request) => {
                    self.round = self.round.wrapping_add(1);
                    let answers = request
                        .questions
                        .iter()
                        .map(|_| Zeroizing::new(String::new()))
                        .collect();
                    PendingPrompt::Authentication(PromptForm {
                        request,
                        answers,
                        focus_first: true,
                    })
                }
            });
        }
        let can_cancel = self.control.can_cancel();
        let action = match &self.prompt {
            Some(PendingPrompt::HostKey(request)) => {
                Some((text!("Доверять и подключиться"), request.deadline))
            }
            Some(PendingPrompt::Authentication(form)) if !form.answers.is_empty() => {
                Some((text!("Отправить ответы"), form.request.deadline))
            }
            _ => None,
        };
        let remaining =
            action.map(|(_, deadline)| deadline.saturating_duration_since(Instant::now()));
        let can_submit = can_cancel && remaining.is_some_and(|remaining| !remaining.is_zero());
        let mut submit = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::unique("sftp_authentication")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 32.0).clamp(180.0, 470.0));
            if let Some(PendingPrompt::HostKey(request)) = &self.prompt {
                ui.heading(request.title);
                ui.label(format!("{} · {}", request.key.address, request.key.algorithm));
                ui.monospace(&request.key.fingerprint);
                let copy = ui.button(text!("Копировать отпечаток"));
                paint_focus(ui, &copy, 5);
                if copy.clicked() {
                    ctx.copy_text(request.key.fingerprint.clone());
                }
                ui.label(text!("Сверьте отпечаток с администратором сервера. PawCommander запомнит его для этого адреса."));
            } else {
                ui.heading(text!("Подключение SFTP"));
                let server = match &self.prompt {
                    Some(PendingPrompt::Authentication(form)) => &form.request.server,
                    _ => &self.target,
                };
                ui.label(server);
            }
            if let Some(PendingPrompt::Authentication(form)) = &mut self.prompt {
                egui::ScrollArea::vertical()
                    .max_height((ctx.content_rect().height() - 190.0).clamp(65.0, 360.0))
                    .show(ui, |ui| {
                        if !form.request.title.is_empty() {
                            ui.heading(&form.request.title);
                        }
                        if !form.request.instructions.is_empty() {
                            ui.label(&form.request.instructions);
                        }
                        ui.add_enabled_ui(can_submit, |ui| {
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
            }
            if let Some(remaining) = remaining.filter(|_| can_submit) {
                let seconds = remaining.as_secs_f64().ceil() as u64;
                ui.label(crate::tr_format!(
                    "{seconds} s to respond", seconds = seconds
                ));
                ctx.request_repaint_after(Duration::from_secs(1));
            }
            if action.is_none() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(text!("Подключение…"));
                });
            }
            ui.horizontal(|ui| {
                let response = ui.add_enabled(can_cancel, egui::Button::new(text!("Отмена")));
                paint_focus(ui, &response, 5);
                cancel = response.clicked();
                if let Some((label, _)) = action {
                    let response = ui.add_enabled(can_submit, egui::Button::new(label));
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
        } else if submit && let Some(prompt) = self.prompt.take() {
            match prompt {
                PendingPrompt::HostKey(request) => request.approve(),
                PendingPrompt::Authentication(form) => form.request.answer(form.answers),
            }
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
    fn sftp_dialog_verifies_the_host_before_showing_authentication_prompts() {
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
        let fingerprint = "SHA256:S6lYGeQoZIplz77dGwpz2PVGsIbn0xk5qqYia5tDR2U";
        let worker = std::thread::spawn(move || {
            auth.confirm_host_key(crate::vfs::HostKey {
                address: "[fixture]:2222".into(),
                algorithm: "ssh-ed25519".into(),
                fingerprint: fingerprint.into(),
            })
            .unwrap();
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
        let click = |job: &mut ConnectionJob, pos| {
            for pressed in [true, false] {
                draw(
                    job,
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
        let trust = wait(&mut job, "Trust and connect");
        assert!(trust.iter().any(|(label, _)| label == fingerprint));
        assert!(trust
            .iter()
            .any(|(label, _)| label == "[fixture]:2222 · ssh-ed25519"));
        assert!(!trust.iter().any(|(label, _)| label == "One-time code"));
        let enter = draw(
            &mut job,
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(enter.iter().any(|(label, _)| label == "Trust and connect"));
        assert!(matches!(job.prompt, Some(PendingPrompt::HostKey(_))));
        click(
            &mut job,
            trust
                .iter()
                .find(|(label, _)| label == "Trust and connect")
                .unwrap()
                .1,
        );
        let first = wait(&mut job, "One-time code");
        assert!(first
            .iter()
            .any(|(label, _)| label.contains("First line\nSecond line")));
        draw(&mut job, vec![]);
        let typed = draw(&mut job, vec![egui::Event::Text(secret.into())]);
        assert!(!typed.iter().any(|(label, _)| label.contains(secret)));
        let Some(PendingPrompt::Authentication(form)) = &job.prompt else {
            panic!("Expected an authentication prompt");
        };
        assert_eq!(form.answers[0].as_str(), secret);
        click(
            &mut job,
            typed
                .iter()
                .find(|(label, _)| label == "Send answers")
                .unwrap()
                .1,
        );
        wait(&mut job, "Confirmation");
        let Some(PendingPrompt::Authentication(form)) = &job.prompt else {
            panic!("Expected the second authentication prompt");
        };
        assert!(form.answers[0].is_empty());
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
