mod document;

use super::{
    comparison::{ComparisonBody, ComparisonDialog, ComparisonResponse},
    file_dialog::{Mode, Picker},
    PawCommander,
};
use crate::{
    i18n::{self, display, text, Language},
    shortcuts::{Command, Shortcuts},
    theme::paint_focus,
};
pub(super) use document::Preferences;
use eframe::egui;
use std::{path::PathBuf, sync::mpsc, thread};

pub(super) struct SettingsTransfer {
    current: Preferences,
    bookmark_keys: Vec<egui::KeyboardShortcut>,
    state: State,
    error: Option<String>,
    ctx: egui::Context,
}

enum State {
    Ready,
    Picking(Picker),
    Reading(mpsc::Receiver<Result<Preferences, String>>),
    Preview(Box<Preferences>),
    Export(PathBuf),
}

impl SettingsTransfer {
    pub(super) fn poll(&mut self) {
        match &self.state {
            State::Picking(picker) => {
                let Some(result) = picker.poll() else { return };
                let mode = picker.mode;
                self.state = State::Ready;
                match result {
                    Ok(Some(path)) => match mode {
                        Mode::Open => self.read(path),
                        Mode::Save { .. } => self.state = State::Export(path),
                    },
                    Ok(None) => {}
                    Err(error) => self.error = Some(error),
                }
            }
            State::Reading(receiver) => {
                let result = match receiver.try_recv() {
                    Ok(result) => result,
                    Err(mpsc::TryRecvError::Empty) => return,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Err(text!("Чтение файла настроек не завершилось.").into())
                    }
                };
                self.state = State::Ready;
                match result {
                    Ok(preferences) => self.state = State::Preview(Box::new(preferences)),
                    Err(error) => self.error = Some(error),
                }
            }
            _ => {}
        }
    }

    fn read(&mut self, path: PathBuf) {
        let (sender, receiver) = mpsc::channel();
        let ctx = self.ctx.clone();
        match thread::Builder::new()
            .name("pawcommander-settings".into())
            .spawn(move || {
                let _ = sender.send(Preferences::load(&path));
                ctx.request_repaint_of(egui::ViewportId::ROOT);
            }) {
            Ok(_) => self.state = State::Reading(receiver),
            Err(error) => {
                self.error = Some(crate::tr_format!(
                    "Could not read the settings file ({error}).",
                    error = error
                ))
            }
        }
    }

    fn choose(&mut self, mode: Mode) {
        self.error = None;
        match Picker::open(mode, &self.ctx) {
            Ok(picker) => self.state = State::Picking(picker),
            Err(error) => self.error = Some(error),
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> ComparisonResponse {
        if let State::Export(path) = &self.state {
            return ComparisonResponse::ExportSettings(
                path.clone(),
                Box::new(self.current.clone()),
            );
        }
        let busy = matches!(self.state, State::Reading(_) | State::Picking(_));
        let modal = egui::Modal::new(egui::Id::unique("settings-transfer")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 44.0).clamp(240.0, 540.0));
            ui.heading(text!("Перенос настроек"));
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            match &self.state {
                State::Picking(_) => { ui.label(text!("Выберите файл в окне macOS.")); }
                State::Reading(_) => { ui.label(text!("Проверка файла настроек…")); }
                State::Preview(preferences) => {
                    let preferences = preferences.as_ref();
                    let conflict = bookmark_conflict(&preferences.shortcuts, &self.bookmark_keys);
                    if conflict {
                        ui.colored_label(ui.visuals().error_fg_color, shortcut_conflict_message());
                    }
                    show_changes(ui, &self.current, preferences);
                    let mut apply = false;
                    let mut cancel = false;
                    ui.horizontal_wrapped(|ui| {
                        let response = ui.add_enabled(
                            !conflict && preferences != &self.current,
                            egui::Button::new(text!("Применить настройки")),
                        );
                        paint_focus(ui, &response, 4);
                        apply = response.clicked();
                        cancel = button(ui, text!("Отмена"));
                    });
                    if apply {
                        return ComparisonResponse::ImportSettings(Box::new(preferences.clone()));
                    }
                    if cancel {
                        self.state = State::Ready;
                    }
                    return ComparisonResponse::Open;
                }
                State::Ready => {
                    ui.label(text!("Файл переносит оформление, сочетания клавиш и общие настройки копирования."));
                    let mut close = false;
                    ui.horizontal_wrapped(|ui| {
                        if button(ui, text!("Экспортировать настройки…")) {
                            self.choose(Mode::Save {
                                suggested_name: "PawCommander-settings.json",
                                message: text!("Выберите имя нового файла настроек."),
                            });
                        }
                        if button(ui, text!("Импортировать настройки…")) {
                            self.choose(Mode::Open);
                        }
                        close = button(ui, text!("Закрыть"));
                    });
                    if close {
                        return ComparisonResponse::Close;
                    }
                }
                State::Export(_) => unreachable!(),
            }
            ComparisonResponse::Open
        });
        if !busy && modal.should_close() {
            ComparisonResponse::Close
        } else {
            modal.inner
        }
    }
}

fn button(ui: &mut egui::Ui, label: &str) -> bool {
    let response = ui.button(label);
    paint_focus(ui, &response, 4);
    response.clicked()
}

fn bookmark_conflict(shortcuts: &Shortcuts, bookmark_keys: &[egui::KeyboardShortcut]) -> bool {
    bookmark_keys
        .iter()
        .any(|key| shortcuts.command(*key).is_some())
}

fn shortcut_conflict_message() -> &'static str {
    text!("Сочетание из файла уже используется для избранного. Измените сочетание перед импортом.")
}

fn show_changes(ui: &mut egui::Ui, current: &Preferences, incoming: &Preferences) {
    fn settings_rows(
        preferences: &Preferences,
        ctx: &egui::Context,
    ) -> Vec<(&'static str, String)> {
        let language = match preferences.language {
            Language::English => "English",
            Language::Russian => "Русский",
            Language::Czech => "Čeština",
        };
        let [left, right] = preferences.pane_grid.map(|grid| {
            if grid {
                text!("Сетка")
            } else {
                text!("Список")
            }
        });
        let [side, stacked] = preferences
            .pane_layout
            .fractions
            .map(|fraction| format!("{}%", display::decimal(f64::from(fraction) * 100.0, 2)));
        let enabled = |value| {
            if value {
                text!("Включено").to_owned()
            } else {
                text!("Выключено").to_owned()
            }
        };
        let rate = match preferences.global_kib {
            0 => text!("Без ограничения").to_owned(),
            rate => format!("{} {}", display::integer(rate), text!("КиБ/с")),
        };
        let mut rows = vec![
            (text!("Язык"), language.into()),
            (text!("Тема оформления"), preferences.theme.name().into()),
            (
                text!("Масштаб интерфейса"),
                format!("{}%", preferences.ui_scale),
            ),
            (
                text!("Высокая контрастность"),
                enabled(preferences.high_contrast),
            ),
            (text!("Две панели"), enabled(preferences.split)),
            (
                text!("Расположение панелей"),
                preferences.pane_layout.orientation.label().into(),
            ),
            (text!("Ширина первой панели"), side),
            (text!("Высота первой панели"), stacked),
            (
                text!("Боковая панель"),
                enabled(preferences.pane_layout.sidebar_visible),
            ),
            (text!("Предпросмотр"), enabled(preferences.preview)),
            (text!("Скрытые файлы"), enabled(preferences.hidden)),
            (text!("Вид первой панели"), left.into()),
            (text!("Вид второй панели"), right.into()),
            (
                text!("Столбцы списка"),
                preferences.list_columns.settings_summary(),
            ),
            (
                text!("Проверять копии (SHA-256)"),
                enabled(preferences.verify_copies),
            ),
            (text!("Общий лимит"), rate),
            (
                text!("Профиль сочетаний"),
                preferences.shortcuts.profile_label().into(),
            ),
        ];
        rows.extend(Command::ALL.into_iter().map(|command| {
            let keys = preferences.shortcuts.keys(ctx, command);
            let keys = if keys.is_empty() {
                text!("Не назначено").to_owned()
            } else {
                keys
            };
            (command.label(), keys)
        }));
        rows
    }

    ui.label(text!("Предпросмотр изменений"));
    if current == incoming {
        ui.label(text!("Все настройки из файла уже применены."));
        return;
    }
    let current = settings_rows(current, ui.ctx());
    let incoming = settings_rows(incoming, ui.ctx());
    let height = (ui.ctx().content_rect().height() * 0.5).max(80.0);
    egui::ScrollArea::vertical()
        .max_height(height)
        .show(ui, |ui| {
            for ((label, before), (_, after)) in current.into_iter().zip(incoming) {
                if before != after {
                    ui.separator();
                    ui.strong(label);
                    ui.label(crate::tr_format!(
                        "From {before} to {after}",
                        before = before,
                        after = after
                    ));
                }
            }
        });
}

impl PawCommander {
    pub(super) fn open_settings_transfer(&mut self, focus: egui::Id, ctx: &egui::Context) {
        let view = SettingsTransfer {
            current: Preferences::capture(&self.settings),
            bookmark_keys: self.settings_bookmark_keys(),
            state: State::Ready,
            error: None,
            ctx: ctx.clone(),
        };
        self.comparison = Some(ComparisonDialog::new(
            ComparisonBody::SettingsTransfer(Box::new(view)),
            Some(focus),
            ctx,
        ));
    }

    fn settings_bookmark_keys(&self) -> Vec<egui::KeyboardShortcut> {
        self.bookmarks
            .iter()
            .filter_map(|bookmark| super::bookmarks::bookmark_slot_key(bookmark.shortcut?))
            .map(|key| crate::shortcuts::chord(key, egui::Modifiers::ALT))
            .collect()
    }

    pub(super) fn export_settings(
        &mut self,
        path: PathBuf,
        preferences: Preferences,
        ctx: &egui::Context,
    ) {
        self.start_job(ctx, text!("Экспорт настроек"), move || {
            preferences.save(&path)?;
            Ok(text!("Настройки экспортированы.").into())
        });
    }

    pub(super) fn import_settings(&mut self, preferences: Preferences, ctx: &egui::Context) {
        if bookmark_conflict(&preferences.shortcuts, &self.settings_bookmark_keys()) {
            self.notice = Some((shortcut_conflict_message().into(), true));
            return;
        }
        if self.settings.split != preferences.split {
            self.toggle_split();
        }
        self.settings.pane_layout = preferences.pane_layout;
        self.settings.preview = preferences.preview;
        self.settings.hidden = preferences.hidden;
        self.settings.grid = preferences.pane_grid[0];
        self.settings.pane_grid = Some(preferences.pane_grid);
        self.settings.list_columns = preferences.list_columns;
        self.settings.shortcuts = preferences.shortcuts;
        self.settings.verify_copies = preferences.verify_copies;
        let mut limits = self.settings.transfer_limits.clone();
        limits.global_kib = preferences.global_kib;
        self.apply_transfer_limits(limits, ctx);
        self.settings.language = preferences.language;
        i18n::set_language(preferences.language);
        self.settings.theme = preferences.theme;
        self.settings.high_contrast = preferences.high_contrast;
        self.settings.theme.apply(ctx, self.settings.high_contrast);
        self.settings.set_ui_scale(ctx, preferences.ui_scale);
        self.preview.refresh();
        self.notice = Some((text!("Настройки импортированы.").into(), false));
        ctx.request_repaint();
    }
}
