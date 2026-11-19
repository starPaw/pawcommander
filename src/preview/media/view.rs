use super::{media_error, session::Session, Command, Frame, Mode, State};
use crate::{i18n::text, operations::Cancellation, vfs::Location};
use eframe::egui;
use std::{
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

pub(in crate::preview) struct Player {
    location: Location,
    controller: Option<Controller>,
    state: State,
    error: Option<String>,
    texture: Option<egui::TextureHandle>,
    seek_widget: Option<egui::Id>,
    dragging_to: Option<f64>,
    seek: Option<(u64, f64)>,
    button: Option<(u64, bool)>,
    ticket: u64,
}

impl Player {
    pub(in crate::preview) fn new(location: Location) -> Self {
        Self {
            location,
            controller: None,
            state: State::default(),
            error: None,
            texture: None,
            seek_widget: None,
            dragging_to: None,
            seek: None,
            button: None,
            ticket: 0,
        }
    }

    pub(in crate::preview) fn seek_focused(&self, ctx: &egui::Context) -> bool {
        self.seek_widget
            .is_some_and(|id| ctx.memory(|memory| memory.has_focus(id)))
    }

    pub(in crate::preview) fn show(&mut self, ui: &mut egui::Ui, cancel: &Cancellation) {
        if self.controller.is_none() && self.error.is_none() {
            match Controller::start(self.location.clone(), cancel.clone(), ui.ctx()) {
                Ok(controller) => self.controller = Some(controller),
                Err(error) => self.error = Some(error),
            }
        }
        self.update(ui.ctx());
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
            return;
        }
        if self.state.mode == Mode::Loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(text("Подготовка медиафайла…"));
            });
            return;
        }
        self.controls(ui);
        ui.separator();
        if let Some(texture) = &self.texture {
            super::super::draw_image(ui, texture, ui.available_height().max(1.0));
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let playing = self
                .button
                .map_or(self.state.playing(), |(_, intent)| intent);
            let label = text(if playing {
                "Пауза"
            } else {
                "Воспроизвести"
            });
            if ui.button(label).clicked() {
                let command = if playing {
                    Command::Pause
                } else {
                    Command::Play
                };
                self.button = self.send(command).map(|ticket| (ticket, !playing));
            }
            if self.state.mode == Mode::Buffering {
                ui.spinner();
                ui.label(text("Буферизация…"));
            }
        });
        let mut seconds = self
            .dragging_to
            .or(self.seek.map(|(_, value)| value))
            .unwrap_or(self.state.position);
        if let Some(duration) = self.state.duration {
            seconds = seconds.min(duration);
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = (ui.available_width() - 132.0).max(40.0);
                let response = ui.add(
                    egui::Slider::new(&mut seconds, 0.0..=duration)
                        .show_value(false)
                        .smart_aim(false),
                );
                ui.ctx()
                    .accesskit_node_builder(response.id, |node| node.set_label(text("Позиция")));
                self.seek_widget = Some(response.id);
                if response.dragged() {
                    self.dragging_to = Some(seconds);
                }
                if response.drag_stopped() || (response.changed() && !response.dragged()) {
                    seconds = self.dragging_to.take().unwrap_or(seconds);
                    self.seek = self
                        .send(Command::Seek(seconds))
                        .map(|ticket| (ticket, seconds));
                }
                ui.label(format!("{} / {}", timestamp(seconds), timestamp(duration)));
            });
        } else {
            self.seek_widget = None;
            ui.label(crate::tr_format!(
                "{} / Длительность недоступна",
                "{} / Duration unavailable",
                timestamp(seconds)
            ));
        }
    }

    fn send(&mut self, command: Command) -> Option<u64> {
        self.ticket = self.ticket.wrapping_add(1);
        if let Some(controller) = &self.controller
            && controller.commands.send((self.ticket, command)).is_ok()
        {
            return Some(self.ticket);
        }
        self.error = Some(media_error());
        None
    }

    fn update(&mut self, ctx: &egui::Context) {
        let Some(controller) = &self.controller else {
            return;
        };
        let Ok(mut mailbox) = controller.latest.lock() else {
            self.error = Some(media_error());
            return;
        };
        let update = std::mem::take(&mut *mailbox);
        drop(mailbox);
        if let Some(error) = update.error {
            self.error = Some(error);
            self.texture = None;
            return;
        }
        if let Some((ticket, state)) = update.state {
            self.state = state;
            if self
                .seek
                .is_some_and(|(pending, _)| ticket >= pending && !state.seeking)
            {
                self.seek = None;
            }
            if self.button.is_some_and(|(pending, _)| ticket >= pending) {
                self.button = None;
            }
        }
        if let Some(Frame { size, rgba }) = update.frame {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                size.map(|dimension| dimension as usize),
                &rgba,
            );
            if let Some(texture) = &mut self.texture {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                self.texture =
                    Some(ctx.load_texture("media-preview", image, egui::TextureOptions::LINEAR));
            }
        }
    }
}

const STARTUP_LIMIT: Duration = Duration::from_secs(12);

#[derive(Default)]
struct Update {
    state: Option<(u64, State)>,
    frame: Option<Frame>,
    error: Option<String>,
}

struct Controller {
    commands: mpsc::Sender<(u64, Command)>,
    latest: Arc<Mutex<Update>>,
    cancel: Cancellation,
}

impl Controller {
    fn start(
        location: Location,
        cancel: Cancellation,
        ctx: &egui::Context,
    ) -> Result<Self, String> {
        let (commands, incoming) = mpsc::channel();
        let latest = Arc::new(Mutex::new(Update::default()));
        let worker_latest = latest.clone();
        let worker_cancel = cancel.clone();
        let ctx = ctx.clone();
        let viewport = ctx.viewport_id();
        thread::Builder::new()
            .name("pawcommander-media".into())
            .spawn(move || {
                let repaint = || ctx.request_repaint_of(viewport);
                if let Err(error) = run(
                    &location,
                    &incoming,
                    &worker_latest,
                    &worker_cancel,
                    &repaint,
                ) && !worker_cancel.is_requested()
                    && let Ok(mut update) = worker_latest.lock()
                {
                    update.error = Some(error);
                    drop(update);
                    repaint();
                }
            })
            .map_err(|_| media_error())?;
        Ok(Self {
            commands,
            latest,
            cancel,
        })
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.cancel.request();
    }
}

fn run(
    location: &Location,
    incoming: &mpsc::Receiver<(u64, Command)>,
    latest: &Mutex<Update>,
    cancel: &Cancellation,
    repaint: &impl Fn(),
) -> Result<(), String> {
    let mut session = Session::open(location, cancel)?;
    let began = Instant::now();
    let mut state = State::default();
    let mut applied = 0;
    while !cancel.is_requested() {
        let delay = if state.playing() || state.seeking || state.mode == Mode::Loading {
            32
        } else {
            120
        };
        let (ticket, command) = match incoming.recv_timeout(Duration::from_millis(delay)) {
            Ok(request) => request,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => (applied, Command::Poll),
        };
        let (next, frame) = session.update(command, cancel)?;
        if next.mode == Mode::Loading && began.elapsed() >= STARTUP_LIMIT {
            return Err(text("Медиаплеер не ответил вовремя.").into());
        }
        if next != state || ticket != applied || frame.is_some() {
            let Ok(mut update) = latest.lock() else {
                return Ok(());
            };
            update.state = Some((ticket, next));
            if frame.is_some() {
                update.frame = frame;
            }
            drop(update);
            repaint();
        }
        applied = ticket;
        state = next;
    }
    Ok(())
}

fn timestamp(seconds: f64) -> String {
    let total = seconds as u64;
    let hours = total / 3600;
    let minutes = (total / 60) % 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ui::ContextExt as _;

    #[test]
    fn keyboard_seek_holds_the_requested_position_until_the_worker_finishes() {
        let ctx = egui::Context::default();
        let cancel = Cancellation::default();
        let latest = Arc::new(Mutex::new(Update::default()));
        let (commands, requested) = mpsc::channel();
        let mut player = Player::new(Location::Local("fixture.mp4".into()));
        player.controller = Some(Controller {
            commands,
            latest: latest.clone(),
            cancel: cancel.clone(),
        });
        let paused = State {
            mode: Mode::Paused,
            position: 2.0,
            duration: Some(12.0),
            seeking: false,
        };
        latest.lock().unwrap().state = Some((0, paused));
        let draw = |player: &mut Player, events| {
            ctx.run_test_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(730.0, 480.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| player.show(ui, &cancel),
            );
        };
        draw(&mut player, vec![]);
        ctx.memory_mut(|memory| memory.request_focus(player.seek_widget.unwrap()));
        draw(&mut player, vec![]);
        assert!(player.seek_focused(&ctx));
        draw(
            &mut player,
            vec![egui::Event::Key {
                key: egui::Key::ArrowRight,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        let (ticket, Command::Seek(position)) = requested.try_recv().unwrap() else {
            panic!("expected a seek from the focused time slider");
        };
        assert!(position > paused.position);

        for (acknowledged, state) in [
            (0, paused),
            (
                ticket,
                State {
                    seeking: true,
                    ..paused
                },
            ),
        ] {
            latest.lock().unwrap().state = Some((acknowledged, state));
            draw(&mut player, vec![]);
            assert_eq!(player.seek, Some((ticket, position)));
        }
        latest.lock().unwrap().state = Some((ticket, State { position, ..paused }));
        draw(&mut player, vec![]);
        assert_eq!(player.seek, None);
        assert_eq!(player.state.position, position);
        assert!(requested.try_recv().is_err());

        assert!(!cancel.is_requested());
        drop(player);
        assert!(cancel.is_requested());
    }
}
