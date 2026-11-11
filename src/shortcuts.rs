use crate::i18n::text;
use eframe::egui::{self, Event, Key, KeyboardShortcut as Chord, Modifiers};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum Command {
    EditPath,
    Filter,
    Search,
    NewTab,
    CloseTab,
    TogglePinTab,
    DuplicateTab,
    ReopenTab,
    Back,
    Forward,
    Parent,
    Open,
    SelectAll,
    Hidden,
    Split,
    OtherPane,
    Preview,
    EditInZed,
    Refresh,
    Properties,
    Rename,
    NewFolder,
    Trash,
    CopyToPane,
    MoveToPane,
    FocusToolbar,
    #[serde(rename = "CommandPalette")]
    Palette,
    FolderJump,
}

impl Command {
    pub const ALL: [Self; 28] = [
        Self::EditPath,
        Self::Filter,
        Self::Search,
        Self::NewTab,
        Self::CloseTab,
        Self::TogglePinTab,
        Self::DuplicateTab,
        Self::ReopenTab,
        Self::Back,
        Self::Forward,
        Self::Parent,
        Self::Open,
        Self::SelectAll,
        Self::Hidden,
        Self::Split,
        Self::OtherPane,
        Self::Preview,
        Self::EditInZed,
        Self::Refresh,
        Self::Properties,
        Self::Rename,
        Self::NewFolder,
        Self::Trash,
        Self::CopyToPane,
        Self::MoveToPane,
        Self::FocusToolbar,
        Self::Palette,
        Self::FolderJump,
    ];

    pub fn label(self) -> &'static str {
        text(match self {
            Self::EditPath => "Ввести путь",
            Self::Filter => "Фильтр текущей папки",
            Self::Search => "Поиск файлов",
            Self::NewTab => "Новая вкладка",
            Self::CloseTab => "Закрыть вкладку",
            Self::TogglePinTab => "Закрепить или открепить вкладку",
            Self::DuplicateTab => "Дублировать вкладку",
            Self::ReopenTab => "Восстановить закрытую вкладку",
            Self::Back => "Назад",
            Self::Forward => "Вперёд",
            Self::Parent => "Родительская папка",
            Self::Open => "Открыть",
            Self::SelectAll => "Выделить всё",
            Self::Hidden => "Скрытые файлы",
            Self::Split => "Две панели",
            Self::OtherPane => "Переключить панель",
            Self::Preview => "Панель сведений",
            Self::EditInZed => "Открыть в Zed",
            Self::Refresh => "Обновить",
            Self::Properties => "Свойства",
            Self::Rename => "Переименовать",
            Self::NewFolder => "Создать папку",
            Self::Trash => "В корзину с подтверждением",
            Self::CopyToPane => "Копировать в соседнюю панель",
            Self::MoveToPane => "Переместить в соседнюю панель",
            Self::FocusToolbar => "Фокус на панели инструментов",
            Self::Palette => "Палитра команд",
            Self::FolderJump => "Быстрый переход к папке",
        })
    }

    // Only dedicated default navigation chords may leave a text field.
    pub fn works_in_text_field(self, chord: Chord) -> bool {
        matches!(
            self,
            Self::EditPath
                | Self::Filter
                | Self::Search
                | Self::FocusToolbar
                | Self::Palette
                | Self::FolderJump
        ) && Profile::Default.chord(self) == Some(chord)
    }
}

const CMD_SHIFT: Modifiers = Modifiers {
    shift: true,
    ..Modifiers::COMMAND
};
const TRASH_ALTERNATE: Chord = Chord::new(Modifiers::COMMAND, Key::Backspace);

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
enum Profile {
    #[default]
    Default,
    Commander,
}

impl Profile {
    fn chord(self, command: Command) -> Option<Chord> {
        use Command::*;
        if self == Self::Commander {
            match command {
                Preview => return Some(Chord::new(Modifiers::NONE, Key::F3)),
                EditInZed => return Some(Chord::new(Modifiers::NONE, Key::F4)),
                CopyToPane => return Some(Chord::new(Modifiers::NONE, Key::F5)),
                MoveToPane => return Some(Chord::new(Modifiers::NONE, Key::F6)),
                NewFolder => return Some(Chord::new(Modifiers::NONE, Key::F7)),
                Trash => return Some(Chord::new(Modifiers::NONE, Key::F8)),
                Refresh => return Some(Chord::new(Modifiers::COMMAND, Key::R)),
                Split => return Some(Chord::new(CMD_SHIFT, Key::D)),
                _ => {}
            }
        }
        let (modifiers, key) = match command {
            EditPath => (Modifiers::COMMAND, Key::L),
            Filter => (Modifiers::COMMAND, Key::F),
            Search => (CMD_SHIFT, Key::F),
            NewTab => (Modifiers::COMMAND, Key::T),
            CloseTab => (Modifiers::COMMAND, Key::W),
            ReopenTab => (CMD_SHIFT, Key::T),
            Back => (Modifiers::ALT, Key::ArrowLeft),
            Forward => (Modifiers::ALT, Key::ArrowRight),
            Parent => (Modifiers::ALT, Key::ArrowUp),
            Open => (Modifiers::NONE, Key::Enter),
            SelectAll => (Modifiers::COMMAND, Key::A),
            Hidden => (Modifiers::COMMAND, Key::H),
            Split => (Modifiers::NONE, Key::F3),
            OtherPane => (Modifiers::NONE, Key::Tab),
            Refresh => (Modifiers::NONE, Key::F5),
            Properties => (Modifiers::ALT, Key::Enter),
            Rename => (Modifiers::NONE, Key::F2),
            NewFolder => (CMD_SHIFT, Key::N),
            Trash => (Modifiers::NONE, Key::Delete),
            FocusToolbar => (Modifiers::NONE, Key::F10),
            Palette => (CMD_SHIFT, Key::P),
            FolderJump => (CMD_SHIFT, Key::J),
            Preview | EditInZed | CopyToPane | MoveToPane | TogglePinTab | DuplicateTab => {
                return None
            }
        };
        Some(Chord::new(modifiers, key))
    }

    fn label(self) -> &'static str {
        match self {
            Self::Default => text("Стандартный"),
            Self::Commander => "Commander",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct Shortcuts {
    profile: Profile,
    // Missing entries follow the profile. None explicitly unbinds a command.
    overrides: BTreeMap<Command, Option<Chord>>,
}

#[derive(Debug, Eq, PartialEq)]
enum BindingError {
    Reserved,
    Conflict(Command),
}

impl Shortcuts {
    fn chord(&self, command: Command) -> Option<Chord> {
        self.overrides
            .get(&command)
            .copied()
            .unwrap_or_else(|| self.profile.chord(command))
    }

    fn alternate(&self, command: Command) -> Option<Chord> {
        (command == Command::Trash && !self.overrides.contains_key(&command))
            .then_some(TRASH_ALTERNATE)
    }

    pub fn command(&self, chord: Chord) -> Option<Command> {
        Command::ALL.into_iter().find(|&command| {
            self.chord(command) == Some(chord) || self.alternate(command) == Some(chord)
        })
    }

    fn set(&mut self, command: Command, chord: Option<Chord>) -> Result<(), BindingError> {
        if let Some(chord) = chord {
            if reserved(chord) {
                return Err(BindingError::Reserved);
            }
            if let Some(other) = self.command(chord).filter(|&other| other != command) {
                return Err(BindingError::Conflict(other));
            }
        }
        if chord != self.chord(command) {
            self.overrides.insert(command, chord);
        }
        Ok(())
    }

    pub fn validate(&mut self) {
        // Keep explicit bindings saved before these default shortcuts existed.
        for command in [
            Command::FocusToolbar,
            Command::Palette,
            Command::FolderJump,
            Command::ReopenTab,
        ] {
            if !self.overrides.contains_key(&command)
                && self
                    .overrides
                    .values()
                    .any(|binding| *binding == self.profile.chord(command))
            {
                self.overrides.insert(command, None);
            }
        }
        // Reject inconsistent saved overrides without discarding other app settings.
        for command in Command::ALL {
            if let Some(chord) = self.overrides.get(&command).copied().flatten() {
                if reserved(chord)
                    || chord.modifiers != normalized(chord.modifiers)
                    || Command::ALL.into_iter().any(|other| {
                        other != command
                            && (self.chord(other) == Some(chord)
                                || self.alternate(other) == Some(chord))
                    })
                {
                    self.overrides.clear();
                    break;
                }
            }
        }
    }

    pub fn keys(&self, ctx: &egui::Context, command: Command) -> String {
        self.chord(command)
            .into_iter()
            .chain(self.alternate(command))
            .map(|chord| ctx.format_shortcut(&chord))
            .collect::<Vec<_>>()
            .join(" / ")
    }

    pub fn hint(&self, ctx: &egui::Context, command: Command, label: &str) -> String {
        let keys = self.keys(ctx, command);
        if keys.is_empty() {
            label.into()
        } else {
            format!("{label} · {keys}")
        }
    }
}

fn normalized(modifiers: Modifiers) -> Modifiers {
    Modifiers {
        alt: modifiers.alt,
        shift: modifiers.shift,
        ctrl: cfg!(target_os = "macos") && modifiers.ctrl,
        mac_cmd: false,
        command: modifiers.command
            || modifiers.mac_cmd
            || (!cfg!(target_os = "macos") && modifiers.ctrl),
    }
}

pub fn chord(key: Key, modifiers: Modifiers) -> Chord {
    Chord::new(normalized(modifiers), key)
}

fn reserved(chord: Chord) -> bool {
    let Chord {
        logical_key: key,
        modifiers: m,
    } = chord;
    // egui-winit consumes clipboard presses before they reach egui.
    if matches!(key, Key::Copy | Key::Cut | Key::Paste)
        || (m.command && matches!(key, Key::C | Key::X | Key::V))
        || (cfg!(target_os = "windows")
            && ((m.shift && key == Key::Delete) || ((m.shift || m.command) && key == Key::Insert)))
    {
        return true;
    }
    if key == Key::Escape
        || (m == Modifiers::SHIFT && key == Key::Tab)
        || (m.alt && key == Key::F4)
        || (m.command && matches!(key, Key::Plus | Key::Equals | Key::Minus | Key::Num0))
    {
        return true;
    }
    if m.alt || m.ctrl || m.command {
        return false;
    }
    // Bare typing keys stay available for text, and arrows retain list navigation.
    let function_key = (Key::F1..=Key::F35).contains(&key);
    !function_key && !matches!(key, Key::Tab | Key::Enter | Key::Delete)
}

pub struct ShortcutEditor {
    draft: Shortcuts,
    recording: Option<Command>,
    error: Option<String>,
}

impl ShortcutEditor {
    pub fn new(shortcuts: &Shortcuts) -> Self {
        Self {
            draft: shortcuts.clone(),
            recording: None,
            error: None,
        }
    }

    pub fn capture(&mut self, ctx: &egui::Context) {
        if let Some(command) = self.recording {
            let mut captured = None;
            ctx.input_mut(|input| {
                input.events.retain(|event| {
                    match event {
                        Event::Key {
                            key,
                            modifiers,
                            pressed: true,
                            repeat: false,
                            ..
                        } => {
                            captured.get_or_insert(chord(*key, *modifiers));
                        }
                        // Clipboard chords can be intercepted by egui-winit.
                        Event::Copy | Event::Cut | Event::Paste(_) => {
                            captured.get_or_insert(Chord::new(Modifiers::NONE, Key::Paste));
                        }
                        Event::Key {
                            key: Key::V,
                            modifiers,
                            pressed: false,
                            ..
                        } if modifiers.command => {
                            captured.get_or_insert(chord(Key::V, *modifiers));
                        }
                        _ => {}
                    }
                    !matches!(
                        event,
                        Event::Key { .. }
                            | Event::Text(_)
                            | Event::Copy
                            | Event::Cut
                            | Event::Paste(_)
                    )
                });
            });
            if let Some(chord) = captured {
                self.recording = None;
                self.error = if chord.logical_key == Key::Escape {
                    None
                } else {
                    self.draft
                        .set(command, Some(chord))
                        .err()
                        .map(|error| match error {
                            BindingError::Reserved => {
                                text("Это сочетание зарезервировано. Выберите другое.").into()
                            }
                            BindingError::Conflict(other) => {
                                let keys = ctx.format_shortcut(&chord);
                                let other = other.label();
                                crate::tr_format!(
                                    "{keys} уже используется для действия «{other}».",
                                    "{keys} is already assigned to “{other}”."
                                )
                            }
                        })
                };
            }
        }
    }

    // Returns true once the user dismisses the modal.
    pub fn show(&mut self, ctx: &egui::Context, shortcuts: &mut Shortcuts) -> bool {
        let mut save = false;
        let mut cancel = false;
        let modal = egui::Modal::new(egui::Id::new("shortcut_editor")).show(ctx, |ui| {
            ui.set_width(490.0);
            ui.heading(text("Горячие клавиши"));
            ui.horizontal(|ui| {
                ui.label(text("Профиль"));
                for profile in [Profile::Default, Profile::Commander] {
                    if ui
                        .selectable_label(self.draft.profile == profile, profile.label())
                        .clicked()
                        && self.draft.profile != profile
                    {
                        self.draft = Shortcuts {
                            profile,
                            ..Shortcuts::default()
                        };
                        self.recording = None;
                        self.error = None;
                    }
                }
                if ui.button(text("Сбросить настройки клавиш")).clicked() {
                    self.draft = Shortcuts::default();
                    self.recording = None;
                    self.error = None;
                }
            });
            ui.small(text("Смена профиля или сброс очищает изменения клавиш."));
            ui.label(text(
                "Нажмите на клавишу действия для записи сочетания. Esc отменяет запись.",
            ));
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height((ctx.screen_rect().height() - 300.0).max(140.0))
                .show(ui, |ui| {
                    egui::Grid::new("shortcut_bindings")
                        .striped(true)
                        .min_col_width(110.0)
                        .spacing([14.0, 6.0])
                        .show(ui, |ui| {
                            for command in Command::ALL {
                                ui.label(command.label());
                                let keys = self.draft.keys(ctx, command);
                                let recording = self.recording == Some(command);
                                let button = if recording {
                                    text("Нажмите сочетание…")
                                } else if keys.is_empty() {
                                    text("Не назначено")
                                } else {
                                    &keys
                                };
                                let response = ui.button(button);
                                if response.clicked() {
                                    self.recording = Some(command);
                                    self.error = None;
                                    response.surrender_focus();
                                }
                                if ui
                                    .add_enabled(
                                        !keys.is_empty(),
                                        egui::Button::new(text("Очистить")),
                                    )
                                    .on_hover_text(command.label())
                                    .clicked()
                                {
                                    let _ = self.draft.set(command, None);
                                    self.recording = None;
                                    self.error = None;
                                }
                                ui.end_row();
                            }
                        });
                });
            ui.separator();
            ui.small(text(
                "Системные сочетания буфера обмена, стрелки ↑/↓ и Esc закреплены.",
            ));
            ui.horizontal(|ui| {
                save = ui
                    .add_enabled(
                        self.recording.is_none(),
                        egui::Button::new(text("Сохранить")),
                    )
                    .clicked();
                cancel = ui.button(text("Отмена")).clicked();
            });
        });
        if save {
            *shortcuts = self.draft.clone();
        }
        let escape = modal.is_top_modal
            && !modal.any_popup_open
            && ctx.input(|input| {
                input.events.iter().any(|event| {
                    matches!(
                        event,
                        Event::Key {
                            key: Key::Escape,
                            pressed: true,
                            repeat: false,
                            modifiers: Modifiers::NONE,
                            ..
                        }
                    )
                })
            });
        save || cancel || modal.backdrop_response.clicked() || escape
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_profiles_have_unique_supported_bindings() {
        for profile in [Profile::Default, Profile::Commander] {
            let shortcuts = Shortcuts {
                profile,
                ..Default::default()
            };
            for command in Command::ALL {
                for key in shortcuts
                    .chord(command)
                    .into_iter()
                    .chain(shortcuts.alternate(command))
                {
                    assert!(!reserved(key), "{profile:?}: {command:?}");
                    assert_eq!(shortcuts.command(key), Some(command), "{profile:?}");
                }
            }
        }
        let commander = Shortcuts {
            profile: Profile::Commander,
            ..Default::default()
        };
        for (key, command) in [
            (Key::F3, Command::Preview),
            (Key::F4, Command::EditInZed),
            (Key::F5, Command::CopyToPane),
            (Key::F6, Command::MoveToPane),
            (Key::F7, Command::NewFolder),
            (Key::F8, Command::Trash),
        ] {
            assert_eq!(
                commander.command(chord(key, Modifiers::NONE)),
                Some(command)
            );
        }
    }

    #[test]
    fn binding_conflicts_reserve_native_chords_and_release_cleared_keys() {
        let mut shortcuts = Shortcuts::default();
        let f2 = chord(Key::F2, Modifiers::NONE);
        let f9 = chord(Key::F9, Modifiers::NONE);
        let shifted_f9 = chord(Key::F9, Modifiers::SHIFT);
        assert_eq!(
            shortcuts.set(Command::Refresh, Some(f2)),
            Err(BindingError::Conflict(Command::Rename))
        );
        assert_eq!(
            shortcuts.set(Command::Refresh, Some(TRASH_ALTERNATE)),
            Err(BindingError::Conflict(Command::Trash))
        );
        for reserved_key in [
            chord(Key::Escape, Modifiers::NONE),
            chord(Key::ArrowDown, Modifiers::SHIFT),
            chord(Key::V, CMD_SHIFT),
            chord(Key::C, Modifiers::COMMAND | Modifiers::ALT),
            chord(Key::Equals, Modifiers::COMMAND),
        ] {
            assert_eq!(
                shortcuts.set(Command::Refresh, Some(reserved_key)),
                Err(BindingError::Reserved)
            );
        }
        assert_eq!(shortcuts, Shortcuts::default());
        shortcuts.set(Command::Rename, Some(f9)).unwrap();
        shortcuts.set(Command::Refresh, Some(shifted_f9)).unwrap();
        assert_eq!(shortcuts.command(f9), Some(Command::Rename));
        assert_eq!(shortcuts.command(shifted_f9), Some(Command::Refresh));
        assert_eq!(shortcuts.command(f2), None);
        shortcuts.set(Command::Trash, None).unwrap();
        shortcuts
            .set(Command::Rename, Some(TRASH_ALTERNATE))
            .unwrap();
        assert_eq!(shortcuts.command(TRASH_ALTERNATE), Some(Command::Rename));
        assert_eq!(shortcuts.command(chord(Key::Delete, Modifiers::NONE)), None);
    }

    #[test]
    fn recording_consumes_keys_and_escape_leaves_the_binding_unchanged() {
        let ctx = egui::Context::default();
        let mut editor = ShortcutEditor::new(&Shortcuts::default());
        let run = |editor: &mut ShortcutEditor, key, modifiers| {
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![
                        Event::Key {
                            key,
                            physical_key: None,
                            pressed: true,
                            repeat: false,
                            modifiers,
                        },
                        Event::Text("typing event".into()),
                    ],
                    ..Default::default()
                },
                |ctx| {
                    editor.capture(ctx);
                    assert!(ctx.input(|input| input.events.is_empty()));
                },
            );
        };
        editor.recording = Some(Command::Rename);
        run(&mut editor, Key::Escape, Modifiers::NONE);
        assert!(editor.recording.is_none());
        assert_eq!(editor.draft, Shortcuts::default());

        editor.recording = Some(Command::Rename);
        run(&mut editor, Key::F9, Modifiers::SHIFT);
        assert!(editor.recording.is_none());
        assert_eq!(
            editor.draft.command(chord(Key::F9, Modifiers::SHIFT)),
            Some(Command::Rename)
        );
    }

    #[test]
    fn saved_overrides_are_validated_against_their_effective_profile() {
        let mut shortcuts = Shortcuts {
            profile: Profile::Commander,
            ..Default::default()
        };
        let f5 = chord(Key::F5, Modifiers::NONE);
        shortcuts.set(Command::CopyToPane, None).unwrap();
        shortcuts.set(Command::Refresh, Some(f5)).unwrap();
        let before = shortcuts.clone();
        shortcuts.validate();
        assert_eq!(shortcuts, before);

        shortcuts.overrides.remove(&Command::CopyToPane);
        shortcuts.validate();
        assert_eq!(shortcuts.command(f5), Some(Command::CopyToPane));
        assert_eq!(shortcuts.profile, Profile::Commander);
    }

    #[test]
    fn new_defaults_preserve_legacy_shortcuts_and_can_be_reassigned() {
        let custom = chord(Key::F9, Modifiers::NONE);
        for profile in [Profile::Default, Profile::Commander] {
            for (key, command) in [
                (Key::P, Command::Palette),
                (Key::J, Command::FolderJump),
                (Key::T, Command::ReopenTab),
            ] {
                let default = chord(key, CMD_SHIFT);
                let mut shortcuts: Shortcuts = serde_json::from_value(serde_json::json!({
                    "profile": profile,
                    "overrides": {"Refresh": default}
                }))
                .unwrap();
                shortcuts.validate();
                assert_eq!(shortcuts.command(default), Some(Command::Refresh));
                assert_eq!(shortcuts.chord(command), None);

                shortcuts.set(command, Some(custom)).unwrap();
                shortcuts.validate();
                let ctx = egui::Context::default();
                assert_eq!(shortcuts.command(custom), Some(command));
                let _ = ctx.run(Default::default(), |ctx| {
                    assert_eq!(shortcuts.keys(ctx, command), ctx.format_shortcut(&custom));
                });
            }
        }
    }
}
