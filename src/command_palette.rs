use crate::i18n::text;
use crate::shortcuts::{Command, Shortcuts};
use crate::vfs::Location;
use eframe::egui;

pub struct CommandPalette {
    pub previous_focus: Option<egui::Id>,
    pub focused_file: Option<Location>,
    query: String,
    highlighted: Option<Command>,
    focus_query: bool,
}

pub enum PaletteOutcome {
    Open,
    Dismissed,
    Run(Command),
}

impl CommandPalette {
    pub fn new(previous_focus: Option<egui::Id>, focused_file: Option<Location>) -> Self {
        Self {
            previous_focus,
            focused_file,
            query: String::new(),
            highlighted: None,
            focus_query: true,
        }
    }

    pub fn refocus_query(&mut self) {
        self.focus_query = true;
    }

    // Keep palette navigation keys away from both the workspace and the query.
    pub fn take_keys(ctx: &egui::Context) -> Vec<egui::Key> {
        let mut keys = Vec::new();
        ctx.input_mut(|input| {
            input.events.retain(|event| {
                let egui::Event::Key {
                    key,
                    pressed: true,
                    repeat,
                    modifiers,
                    ..
                } = *event
                else {
                    return true;
                };
                use egui::{Key, Modifiers};
                match (modifiers, key) {
                    (Modifiers::NONE, Key::Tab) => keys.push(Key::ArrowDown),
                    (Modifiers::SHIFT, Key::Tab) => keys.push(Key::ArrowUp),
                    (Modifiers::NONE, Key::ArrowUp | Key::ArrowDown) => keys.push(key),
                    (Modifiers::NONE, Key::Enter | Key::Escape) if !repeat => keys.push(key),
                    (Modifiers::NONE, Key::Enter | Key::Escape) => {}
                    _ => return true,
                }
                false
            });
        });
        keys
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        shortcuts: &Shortcuts,
        keys: &[egui::Key],
        is_available: impl Fn(Command) -> bool,
    ) -> PaletteOutcome {
        let modal = egui::Modal::new(egui::Id::unique("command_palette")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 40.0).clamp(160.0, 500.0));
            ui.heading(Command::Palette.label());
            let query_id = ui.make_persistent_id("query");
            let navigate = keys
                .iter()
                .any(|key| matches!(key, egui::Key::ArrowUp | egui::Key::ArrowDown));
            if self.focus_query || navigate {
                ui.memory_mut(|memory| memory.request_focus(query_id));
                self.focus_query = ui.is_sizing_pass();
            }
            let query = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .id(query_id)
                    .hint_text(text!("Поиск команд…"))
                    .desired_width(ui.available_width())
                    .lock_focus(true)
                    .return_key(None),
            );
            ctx.accesskit_node_builder(query.id, |node| {
                node.set_label(text!("Поиск команд…"));
            });

            let matches = matching_commands(&self.query);
            let available: Vec<_> = matches
                .iter()
                .copied()
                .filter(|&command| is_available(command))
                .collect();
            let old_highlight = self.highlighted;
            let mut index = if query.changed() {
                None
            } else {
                available
                    .iter()
                    .position(|&command| Some(command) == self.highlighted)
            }
            .or_else(|| (!available.is_empty()).then_some(0));
            let mut outcome = PaletteOutcome::Open;
            for &key in keys {
                if key == egui::Key::Escape {
                    outcome = PaletteOutcome::Dismissed;
                    break;
                }
                let Some(current) = index else {
                    continue;
                };
                match key {
                    egui::Key::ArrowUp => {
                        index = Some((current + available.len() - 1) % available.len());
                    }
                    egui::Key::ArrowDown => index = Some((current + 1) % available.len()),
                    egui::Key::Enter => {
                        outcome = PaletteOutcome::Run(available[current]);
                        break;
                    }
                    _ => {}
                }
            }
            self.highlighted = index.map(|index| available[index]);
            if matches.is_empty() {
                ui.label(text!("Команды не найдены"));
            }
            egui::ScrollArea::vertical()
                .max_height((ctx.content_rect().height() - 140.0).clamp(60.0, 300.0))
                .show(ui, |ui| {
                    for command in matches {
                        let highlighted = self.highlighted == Some(command);
                        let keys = shortcuts.keys(ctx, command);
                        let response = ui
                            .push_id(command, |ui| {
                                ui.add_enabled(
                                    available.contains(&command),
                                    egui::Button::new(command.label())
                                        .shortcut_text(&keys)
                                        .selected(highlighted)
                                        .truncate()
                                        .min_size(egui::vec2(ui.available_width(), 24.0)),
                                )
                            })
                            .inner;
                        ctx.accesskit_node_builder(response.id, |node| {
                            // Shortcut text is a separate atom in newer egui versions.
                            // Keep the spoken command name separate from its shortcut.
                            node.set_label(command.label());
                            node.set_selected(highlighted);
                            if !keys.is_empty() {
                                node.set_keyboard_shortcut(keys);
                            }
                        });
                        if highlighted && self.highlighted != old_highlight {
                            response.scroll_to_me(Some(egui::Align::Center));
                        }
                        if response.gained_focus() && !navigate {
                            self.highlighted = Some(command);
                        }
                        if response.clicked() && matches!(outcome, PaletteOutcome::Open) {
                            outcome = PaletteOutcome::Run(command);
                        }
                    }
                });
            ui.small(text!(
                "Вверх/вниз — выбрать · Enter — выполнить · Esc — закрыть",
            ));
            outcome
        });
        if modal.backdrop_response.clicked() {
            PaletteOutcome::Dismissed
        } else {
            modal.inner
        }
    }
}

fn matching_commands(query: &str) -> Vec<Command> {
    let query = query.to_lowercase();
    Command::ALL
        .into_iter()
        .filter(|&command| command != Command::Palette)
        .filter(|command| {
            let label = command.label().to_lowercase();
            query.split_whitespace().all(|word| label.contains(word))
        })
        .collect()
}
