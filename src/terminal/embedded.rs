use crate::{i18n::text, vfs::Location};
use alacritty_terminal::grid::Scroll;
use eframe::egui;
use session::{Session, State};

mod input;
mod palette;
mod screen;
pub(super) mod session;

pub(crate) struct Panel {
    folder: Location,
    session: Session,
    close_requested: bool,
    focus_requested: bool,
    copy: Option<String>,
    input_full: bool,
    scroll: f32,
}

impl Panel {
    pub(crate) fn open(folder: Location, ctx: &egui::Context) -> Result<Self, String> {
        let session = Session::start(folder.clone(), ctx).map_err(|error| error.to_string())?;
        ctx.request_repaint();
        Ok(Self {
            folder,
            session,
            close_requested: false,
            focus_requested: true,
            copy: None,
            input_full: false,
            scroll: 0.0,
        })
    }

    pub(crate) fn finished(&self) -> bool {
        self.session.state.finished()
    }

    pub(crate) fn focus(&mut self, ctx: &egui::Context) {
        if !self.close_requested {
            self.focus_requested = true;
            ctx.request_repaint();
        }
    }

    pub(crate) fn poll(&mut self) -> bool {
        self.session.poll();
        self.close_requested && self.finished()
    }

    pub(crate) fn shutdown(&mut self) {
        self.session.shutdown();
    }

    pub(crate) fn capture(
        &mut self,
        ctx: &egui::Context,
        events: &mut Vec<egui::Event>,
        keep_shortcut: impl Fn(egui::Key, egui::Modifiers) -> bool,
    ) -> bool {
        if !ctx.memory(|memory| memory.has_focus(input_id())) {
            return false;
        }
        let running = !self.close_requested && !self.finished();
        let mode = *self.session.terminal.lock().mode();
        events.retain(|event| {
            let bytes = match event {
                egui::Event::Key { key, modifiers, .. } if keep_shortcut(*key, *modifiers) => {
                    return true
                }
                egui::Event::Copy => {
                    self.copy = self.session.terminal.lock().selection_to_string();
                    return false;
                }
                egui::Event::Text(text) | egui::Event::Ime(egui::ImeEvent::Commit(text))
                    if running =>
                {
                    text.chars()
                        .filter(|character| !character.is_control())
                        .collect::<String>()
                        .into_bytes()
                }
                egui::Event::Paste(text) if running => {
                    if text.len() > session::MAX_PENDING_INPUT {
                        self.input_full = true;
                        return false;
                    }
                    input::paste(text, mode)
                }
                egui::Event::Key {
                    key,
                    modifiers,
                    pressed: true,
                    ..
                } if running => input::key_bytes(*key, *modifiers, mode).unwrap_or_default(),
                egui::Event::Key { .. }
                | egui::Event::Paste(_)
                | egui::Event::PasteImage(_)
                | egui::Event::Text(_)
                | egui::Event::Ime(_)
                | egui::Event::Cut => return false,
                _ => return true,
            };
            if !bytes.is_empty() {
                self.input_full = !self.session.input(&bytes);
                if !self.input_full {
                    self.session.terminal.lock().scroll_display(Scroll::Bottom);
                }
            }
            false
        });
        true
    }

    pub(crate) fn show(&mut self, ui: &mut egui::Ui, interactive: bool) {
        let ctx = ui.ctx().clone();
        let colors = crate::theme::Palette::from_ctx(&ctx);
        let max_height = (ui.available_height() * 0.65).max(112.0);
        let mut close = false;
        egui::Panel::bottom("embedded_terminal")
            .size_range(112.0..=max_height)
            .default_size(260.0_f32.min(max_height))
            .resizable(interactive)
            .frame(
                egui::Frame::new()
                    .fill(colors.surface)
                    .inner_margin(egui::Margin::symmetric(8, 5)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(text!("Терминал"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        close = ui
                            .add_enabled(
                                interactive && !self.close_requested,
                                egui::Button::new(text!("Закрыть терминал")),
                            )
                            .clicked();
                        let folder = self.folder.display();
                        let origin =
                            crate::tr_format!("Папка запуска · {folder}", "Started in · {folder}");
                        ui.add(egui::Label::new(&origin).truncate())
                            .on_hover_text(origin);
                    });
                });
                if self.close_requested {
                    ui.label(text!("Завершение сеанса терминала…"));
                } else {
                    match &self.session.state {
                        State::Starting => {
                            ui.label(text!("Запуск сеанса терминала…"));
                        }
                        State::Running => {}
                        State::Exited(status) => {
                            let message = status.code().map_or_else(
                                || text!("Процесс терминала завершён.").into(),
                                |code| {
                                    crate::tr_format!(
                                        "Процесс терминала завершился с кодом {code}.",
                                        "The terminal process exited with code {code}."
                                    )
                                },
                            );
                            ui.label(message);
                        }
                        State::Stopped => {
                            ui.label(text!("Процесс терминала завершён."));
                        }
                        State::Failed(error) => {
                            ui.colored_label(colors.error, error);
                        }
                    }
                    if self.input_full {
                        ui.colored_label(colors.error, text!("Очередь ввода терминала заполнена."));
                    }
                }
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), ui.available_height().max(20.0)),
                    egui::Sense::hover(),
                );
                let response = ui.interact(
                    rect,
                    input_id(),
                    if interactive {
                        egui::Sense::click_and_drag()
                    } else {
                        egui::Sense::hover()
                    },
                );
                if interactive
                    && (std::mem::take(&mut self.focus_requested)
                        || response.clicked()
                        || response.drag_started())
                {
                    response.request_focus();
                }
                let grid = screen::Screen::measure(ui, rect);
                self.session.resize(grid.size);
                if interactive {
                    grid.interact(
                        ui,
                        &response,
                        &mut self.session.terminal.lock(),
                        &mut self.scroll,
                    );
                    if let Some(copy) = self.copy.take() {
                        ctx.copy_text(copy);
                    }
                }
                grid.paint(
                    ui,
                    &self.session,
                    interactive && response.has_focus() && ctx.input(|input| input.focused),
                );
            });
        if close {
            ctx.memory_mut(|memory| memory.surrender_focus(input_id()));
            self.close_requested = true;
            self.session.stop();
            ctx.request_repaint();
        }
    }
}

fn input_id() -> egui::Id {
    egui::Id::unique("embedded_terminal_input")
}

#[cfg(test)]
mod tests;
