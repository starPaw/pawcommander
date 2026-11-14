use super::actions::SelectionChange;
use super::selection::{Rule, RuleKind};
use super::state::Settings;
use super::{file_name, PawCommander};
use crate::shortcuts::Command;
use crate::theme::Palette;
use crate::vfs::{Endpoint, FileEntry, Location};
use crate::{icons, vfs as fs_ops};
use eframe::egui;
use eframe::egui::{Color32, RichText};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

pub(super) struct SelectionScan {
    pub(super) rule: Rule,
    pub(super) rows: Rc<Vec<usize>>,
    pub(super) next: usize,
    pub(super) matches: BTreeSet<Location>,
}

impl SelectionScan {
    pub(super) fn new(rule: Rule, rows: Rc<Vec<usize>>) -> Self {
        Self {
            rule,
            rows,
            next: 0,
            matches: BTreeSet::new(),
        }
    }

    pub(super) fn step(&mut self, entries: &[FileEntry], deadline: Instant) -> bool {
        while let Some(&index) = self.rows.get(self.next) {
            if self.next.is_multiple_of(32) && Instant::now() >= deadline {
                return false;
            }
            let entry = &entries[index];
            if self.rule.matches(entry) {
                self.matches.insert(entry.path.clone());
            }
            self.next += 1;
        }
        true
    }
}

pub(super) enum Dialog {
    SelectionRule {
        command: Command,
        kind: RuleKind,
        tab: u64,
        location: Location,
        input: String,
        scan: Option<SelectionScan>,
    },
    NewFolder {
        parent: Location,
        name: String,
    },
    Rename {
        source: Location,
        name: String,
    },
    Trash {
        paths: Vec<Location>,
    },
    WorkspaceName {
        editing: Option<usize>,
        name: String,
    },
}

#[derive(Clone)]
pub(super) struct SftpForm {
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) user: String,
    pub(super) path: String,
    pub(super) key: String,
    pub(super) password: String,
    pub(super) passphrase: String,
    pub(super) use_saved: bool,
    pub(super) save_key: bool,
    pub(super) error: Option<String>,
}

impl Default for SftpForm {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            user: String::new(),
            path: ".".into(),
            key: String::new(),
            password: String::new(),
            passphrase: String::new(),
            use_saved: true,
            save_key: false,
            error: None,
        }
    }
}

pub(super) struct ConnectionJob {
    pub(super) pane: usize,
    pub(super) save_profile: bool,
    pub(super) result: Receiver<Result<Location, String>>,
}

impl PawCommander {
    pub(super) fn remember_dialog_file_focus(&mut self, ctx: &egui::Context) {
        self.dialog_file_focus = self
            .file_row_focus(ctx)
            .map(|(pane, path)| (self.panes[pane].tab().id, path.clone()))
            .or_else(|| {
                let tab = self.tab();
                tab.focused_cursor(ctx)?;
                Some((tab.id, tab.cursor.clone()?))
            });
    }

    pub(super) fn restore_dialog_file_focus(&mut self, ctx: &egui::Context) {
        if self.dialog.is_some() {
            return;
        }
        let Some((tab_id, path)) = self.dialog_file_focus.take() else {
            return;
        };
        if self.keyboard_blocked(ctx) || self.tab().id != tab_id {
            return;
        }
        let tab = self.tab();
        let id = tab.entry_id(&path);
        let target = tab
            .rendered_rows
            .contains_key(&id)
            .then_some(id)
            .or_else(|| {
                tab.visible(self.folder_view(self.active))
                    .iter()
                    .map(|&index| tab.entry_id(&tab.entries[index].path))
                    .find(|id| tab.rendered_rows.contains_key(id))
            });
        if let Some(id) = target {
            // Render has run, so the just-closed modal cannot clear this request.
            ctx.memory_mut(|memory| memory.request_focus(id));
            ctx.request_repaint();
        }
    }

    pub(super) fn accessibility_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_accessibility {
            return;
        }
        let response = egui::Modal::new(egui::Id::unique("accessibility")).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.heading(crate::i18n::text("Доступность"));
            let contrast = ui.checkbox(
                &mut self.settings.high_contrast,
                crate::i18n::text("Высокая контрастность"),
            );
            if self.accessibility_focus && !ui.is_sizing_pass() {
                contrast.request_focus();
                self.accessibility_focus = false;
            }
            if contrast.changed() {
                self.settings.theme.apply(ctx, self.settings.high_contrast);
            }
            ui.add_space(6.0);
            ui.label(crate::i18n::text("Масштаб интерфейса"));
            for scale in Settings::UI_SCALES {
                let response = ui.radio(self.settings.ui_scale == scale, format!("{scale}%"));
                if response.clicked() {
                    self.settings.set_ui_scale(ctx, scale);
                }
            }
            ui.add_space(6.0);
            ui.button(crate::i18n::text("Закрыть")).clicked()
        });
        if response.inner || response.should_close() {
            self.show_accessibility = false;
            self.focus_toolbar = true;
            ctx.request_repaint();
        }
    }

    pub(super) fn sftp_dialog(&mut self, ctx: &egui::Context) {
        let colors = Palette::from_ctx(ctx);
        let Some(form) = &mut self.sftp_form else {
            return;
        };
        let busy = self.connection.is_some() || self.job.is_some();
        let mut submit = false;
        let mut cancel = false;
        let mut forget = false;
        egui::Modal::new(egui::Id::unique("sftp_connect")).show(ctx, |ui| {
            ui.set_width(470.0);
            ui.heading(crate::i18n::text("Подключение SFTP"));
            ui.add_space(10.0);
            ui.add_enabled_ui(!busy, |ui| {
                egui::ComboBox::from_id_salt("saved_sftp_profiles").selected_text(crate::i18n::text("Сохранённые подключения")).show_ui(ui, |ui| {
                    for profile in &self.settings.sftp_profiles {
                        if ui.selectable_label(false, profile.display()).clicked()
                            && let Location::Sftp { endpoint, path } = profile {
                                form.host = endpoint.host.clone(); form.port = endpoint.port; form.user = endpoint.user.clone();
                                form.key = endpoint.key.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                                form.path = path.display().to_string(); form.password.clear(); form.passphrase.clear();
                                form.use_saved = true; form.save_key = false; form.error = None;
                            }
                    }
                });
                egui::Grid::new("sftp_fields").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    let label = ui.label(crate::i18n::text("Сервер"));
                    ui.text_edit_singleline(&mut form.host).labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label(crate::i18n::text("Порт"));
                    ui.add(egui::DragValue::new(&mut form.port).range(1..=65535)).labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label(crate::i18n::text("Пользователь"));
                    ui.text_edit_singleline(&mut form.user).labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label(crate::i18n::text("Начальная папка"));
                    ui.text_edit_singleline(&mut form.path).labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label(crate::i18n::text("Пароль"));
                    ui.add(egui::TextEdit::singleline(&mut form.password).password(true)).labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label(crate::i18n::text("Приватный ключ"));
                    ui.add(egui::TextEdit::singleline(&mut form.key).hint_text(crate::i18n::text("Путь к ключу (необязательно)"))).labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label(crate::i18n::text("Пароль ключа"));
                    ui.add(egui::TextEdit::singleline(&mut form.passphrase).password(true)).labelled_by(label.id);
                    ui.end_row();
                });
                ui.add_space(10.0);
                ui.checkbox(&mut form.use_saved, crate::i18n::text("Использовать сохранённый ключ и фразу"));
                ui.checkbox(&mut form.save_key, crate::i18n::text("Сохранить ключ и фразу после подключения"));
                ui.weak(crate::i18n::text("Ключ и его фраза сохраняются в системном хранилище секретов. Пароль аккаунта не сохраняется. При сохранении читается выбранный файл ключа. ~/.ssh/config не читается."));
                forget = ui.button(crate::i18n::text("Удалить сохранённый секрет")).clicked();
                ui.weak(crate::i18n::text("Ключ сервера должен быть проверен и сохранён в ~/.ssh/known_hosts через OpenSSH."));
                if let Some(error) = &form.error { ui.colored_label(colors.error, error); }
                ui.horizontal(|ui| {
                    cancel = ui.button(crate::i18n::text("Отмена")).clicked();
                    submit = ui.add_enabled(!form.host.trim().is_empty() && !form.user.trim().is_empty(), egui::Button::new(crate::i18n::text("Подключиться"))).clicked();
                });
            });
            if busy { ui.horizontal(|ui| { ui.spinner(); ui.label(crate::i18n::text("Подключение…")); }); }
        });
        if submit {
            let endpoint = Endpoint {
                host: form.host.trim().into(),
                port: form.port,
                user: form.user.trim().into(),
                key: if form.key.is_empty() {
                    None
                } else {
                    Some(if let Some(rest) = form.key.strip_prefix("~/") {
                        dirs::home_dir().unwrap_or_default().join(rest)
                    } else {
                        PathBuf::from(&form.key)
                    })
                },
            };
            let path = form.path.clone();
            let password = std::mem::take(&mut form.password);
            let passphrase = std::mem::take(&mut form.passphrase);
            form.error = None;
            let (tx, rx) = mpsc::channel();
            self.connection = Some(ConnectionJob {
                pane: self.active,
                save_profile: form.save_key,
                result: rx,
            });
            let use_saved = form.use_saved;
            let save_key = form.save_key;
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(fs_ops::connect_location(
                    endpoint, &path, password, passphrase, use_saved, save_key,
                ));
                ctx.request_repaint();
            });
        } else if cancel {
            self.sftp_form = None;
        } else if forget {
            let endpoint = Endpoint {
                host: form.host.trim().into(),
                port: form.port,
                user: form.user.trim().into(),
                key: if form.key.is_empty() {
                    None
                } else {
                    Some(if let Some(rest) = form.key.strip_prefix("~/") {
                        dirs::home_dir().unwrap_or_default().join(rest)
                    } else {
                        PathBuf::from(&form.key)
                    })
                },
            };
            form.passphrase.clear();
            self.sftp_form = None;
            self.start_job(ctx, crate::i18n::text("Удаление сохранённого секрета"), move || {
                crate::credentials::forget(&endpoint)?;
                Ok(crate::i18n::text("Сохранённый ключ и фраза удалены. Исходный файл ключа и открытое SSH-соединение сохранены.").into())
            });
        }
    }

    pub(super) fn dialogs(&mut self, ctx: &egui::Context) {
        let colors = Palette::from_ctx(ctx);
        if self.show_help {
            egui::Window::new(crate::i18n::text("О PawCommander")).collapsible(false).resizable(false)
                .vscroll(true).max_height((ctx.content_rect().height() - 64.0).max(100.0))
                .open(&mut self.show_help).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(icons::logo(96.0));
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.heading(crate::i18n::text("Ваши файлы. Две панели."));
                        ui.label(crate::i18n::text("Файловый менеджер на Rust, вдохновлённый Dolphin."));
                    });
                });
                ui.add_space(12.0);
                ui.label(crate::i18n::text("Действующие сочетания показаны в разделе Настройки → Горячие клавиши."));
                ui.add_space(14.0); ui.label(crate::i18n::text("Для локальных файлов используется системный буфер, если он доступен. Для SFTP используется буфер PawCommander."));
                #[cfg(target_os = "macos")]
                ui.label(crate::i18n::text("Вставка файлов срабатывает при нажатии V. Отпускайте Cmd и Option в любом порядке. В текстовых полях работают обычные Cmd+C, Cmd+X и Cmd+V."));
                ui.label(crate::i18n::text("При совпадении имён выберите действие в диалоге. Удаление SFTP — навсегда с подтверждением; локальные файлы отправляются в корзину."));
            });
        }
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        let mut cancel = false;
        let mut confirm = false;
        let title = match &dialog {
            Dialog::SelectionRule { command, .. } => command.label(),
            Dialog::NewFolder { .. } => crate::i18n::text("Новая папка"),
            Dialog::Rename { .. } => crate::i18n::text("Переименовать"),
            Dialog::WorkspaceName { editing, .. } => {
                if editing.is_some() {
                    crate::i18n::text("Переименовать рабочее пространство")
                } else {
                    crate::i18n::text("Сохранить пару папок")
                }
            }
            Dialog::Trash { paths } => {
                if paths.iter().any(Location::remote) {
                    crate::i18n::text("Удалить с сервера навсегда?")
                } else {
                    crate::i18n::text("Отправить в корзину?")
                }
            }
        };
        // The modal blocks file interactions while a concrete destination is being confirmed.
        egui::Modal::new(egui::Id::unique("file_dialog")).show(ctx, |ui| {
            ui.set_width(430.0);
            ui.heading(title);
            ui.add_space(12.0);
            match &mut dialog {
                Dialog::SelectionRule { kind, input, scan, .. } => {
                    let (field, hint) = match kind {
                        RuleKind::Mask => (crate::i18n::text("Маска имени файла"), "photo-?.jpg"),
                        RuleKind::Extension => (crate::i18n::text("Расширение"), "txt"),
                    };
                    let label = ui.label(field);
                    let response = ui
                        .add_enabled(
                            scan.is_none(),
                            egui::TextEdit::singleline(input)
                                .hint_text(hint)
                                .desired_width(f32::INFINITY)
                                .return_key(None),
                        )
                        .labelled_by(label.id);
                    if self.dialog_focus {
                        response.request_focus();
                    }
                    if matches!(kind, RuleKind::Mask) {
                        ui.weak(crate::i18n::text("Звёздочка * заменяет любое число символов, знак ? — один символ."));
                    }
                    if let Some(scan) = scan {
                        let progress = scan.next as f32 / scan.rows.len().max(1) as f32;
                        ui.add(egui::ProgressBar::new(progress)
                            .text(crate::i18n::text("Поиск подходящих объектов…")));
                    } else if !input.is_empty() && kind.pattern(input).is_none() {
                        let error = match kind {
                            RuleKind::Mask => "Длина маски не должна превышать 1024 байта.",
                            RuleKind::Extension => "Введите одно расширение, например txt или .txt.",
                        };
                        ui.colored_label(colors.error, crate::i18n::text(error));
                    }
                }
                Dialog::NewFolder { parent, name } => {
                    ui.label(RichText::new(parent.display().to_string()).color(colors.muted));
                    let response = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text(crate::i18n::text("Имя папки"))
                            .desired_width(f32::INFINITY),
                    );
                    ctx.accesskit_node_builder(response.id, |node| node.set_label(crate::i18n::text("Имя папки")));
                    if self.dialog_focus {
                        response.request_focus();
                    }
                }
                Dialog::Rename { source, name } => {
                    ui.label(RichText::new(source.display().to_string()).color(colors.muted));
                    let response =
                        ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                    ctx.accesskit_node_builder(response.id, |node| node.set_label(crate::i18n::text("Имя файла")));
                    if self.dialog_focus {
                        response.request_focus();
                    }
                }
                Dialog::WorkspaceName { editing, name } => {
                    if editing.is_none() {
                        for (label, pane) in [
                            (crate::i18n::text("Левая папка"), &self.panes[0]),
                            (crate::i18n::text("Правая папка"), &self.panes[1]),
                        ] {
                            ui.label(crate::tr_format!(
                                "{label}: {}",
                                "{label}: {}",
                                pane.tab().path.display()
                            ));
                        }
                    }
                    let label = ui.label(crate::i18n::text("Имя рабочего пространства"));
                    let response = ui
                        .add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY))
                        .labelled_by(label.id);
                    if self.dialog_focus {
                        response.request_focus();
                    }
                    if self.workspace_name_taken(name, *editing) {
                        ui.colored_label(
                            colors.error,
                            crate::i18n::text("Это имя уже используется."),
                        );
                    }
                }
                Dialog::Trash { paths } => {
                    ui.label(crate::tr_format!("Выбрано объектов: {}", "Selected items: {}", paths.len()));
                    for p in paths.iter().take(6) {
                        ui.label(file_name(p));
                    }
                    if paths.len() > 6 {
                        ui.label(crate::tr_format!("…и ещё {}", "…and {} more", paths.len() - 6));
                    }
                    ui.add_space(8.0);
                    ui.label(if paths.iter().any(Location::remote) { crate::i18n::text("Файлы и содержимое папок будут удалены с сервера без возможности восстановления из корзины.") } else { crate::i18n::text("Их можно будет восстановить из системной корзины.") });
                }
            }
            ui.add_space(16.0);
            let valid = match &dialog {
                Dialog::SelectionRule { kind, input, scan, .. } => {
                    scan.is_none() && kind.pattern(input).is_some()
                }
                Dialog::NewFolder { name, .. } | Dialog::Rename { name, .. } => {
                    !name.trim().is_empty()
                }
                Dialog::WorkspaceName { editing, name } => {
                    !name.trim().is_empty() && !self.workspace_name_taken(name, *editing)
                }
                Dialog::Trash { .. } => true,
            };
            ui.horizontal(|ui| {
                let cancel_button = ui.button(crate::i18n::text("Отмена"));
                if self.dialog_focus && matches!(dialog, Dialog::Trash { .. }) {
                    cancel_button.request_focus();
                }
                cancel = cancel_button.clicked();
                let label = if matches!(dialog, Dialog::Trash { .. }) {
                    if matches!(&dialog, Dialog::Trash { paths } if paths.iter().any(Location::remote)) { crate::i18n::text("Удалить навсегда") } else { crate::i18n::text("В корзину") }
                } else if matches!(dialog, Dialog::SelectionRule { .. }) {
                    crate::i18n::text("Выделить")
                } else {
                    crate::i18n::text("Сохранить")
                };
                let button = if self.settings.high_contrast {
                    egui::Button::new(label)
                } else {
                    egui::Button::new(RichText::new(label).color(Color32::WHITE))
                        .fill(Color32::from_rgb(45, 105, 220))
                };
                confirm = ui.add_enabled(valid, button).clicked();
            });
            let submit_on_enter = match dialog {
                Dialog::Trash { .. } => false,
                // The Enter that opens the dialog belongs to its sidebar control.
                Dialog::WorkspaceName { .. } | Dialog::SelectionRule { .. } => !self.dialog_focus,
                _ => true,
            };
            if submit_on_enter && valid && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                confirm = true;
            }
            self.dialog_focus = false;
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel = true;
            }
        });
        if matches!(dialog, Dialog::SelectionRule { .. }) && (confirm || cancel) {
            let key = if cancel {
                egui::Key::Escape
            } else {
                egui::Key::Enter
            };
            if ctx.input(|input| input.key_pressed(key)) {
                self.guard_picker_close_key(ctx, &[key], key);
            }
        }
        if let Dialog::SelectionRule {
            kind,
            tab,
            location,
            input,
            scan,
            ..
        } = &mut dialog
        {
            if cancel {
                ctx.request_repaint();
                return;
            }
            if scan.is_none() && !confirm {
                self.dialog = Some(dialog);
                return;
            }
            let current = self.tab();
            let rows = current.visible(self.folder_view(self.active));
            if current.id != *tab
                || current.path != *location
                || scan
                    .as_ref()
                    .is_some_and(|scan| !Rc::ptr_eq(&scan.rows, &rows))
            {
                self.notice = Some((
                    crate::i18n::text("Папка или список изменились. Повторите выделение.").into(),
                    false,
                ));
                ctx.request_repaint();
                return;
            }
            if confirm {
                *scan = kind
                    .compile(input)
                    .map(|rule| SelectionScan::new(rule, rows));
            }
            if let Some(scan) = scan
                && scan.step(&current.entries, Instant::now() + Duration::from_millis(4))
            {
                self.apply_selection(
                    SelectionChange::Matches(std::mem::take(&mut scan.matches)),
                    ctx,
                );
                return;
            }
            self.dialog = Some(dialog);
            ctx.request_repaint();
            return;
        }
        if confirm && !cancel {
            match dialog {
                Dialog::SelectionRule { .. } => unreachable!("rule handled above"),
                Dialog::NewFolder { parent, name } => self.start_described_job(
                    ctx,
                    crate::i18n::text("Создание папки"),
                    format!(
                        "{}: {}",
                        crate::i18n::text("Создание папки"),
                        parent.join(&name)
                    ),
                    move |progress| {
                        let target = parent.join(&name);
                        progress
                            .results
                            .plan(vec![(parent.clone(), Some(target.display()))]);
                        progress.results.begin(&parent);
                        progress.item(&parent.path().join(&name));
                        fs_ops::create_directory(&parent, &name).map(|p| {
                            progress.results.target(&parent, p.display());
                            crate::tr_format!(
                                "Папка создана: {}",
                                "Folder created: {}",
                                p.display()
                            )
                        })
                    },
                ),
                Dialog::Rename { source, name } => self.start_described_job(
                    ctx,
                    crate::i18n::text("Переименование"),
                    format!(
                        "{}: {} → {}",
                        crate::i18n::text("Переименование"),
                        source,
                        name
                    ),
                    move |progress| {
                        fs_ops::rename_with_undo(&source, &name, progress).map(|p| {
                            crate::tr_format!("Переименовано: {}", "Renamed: {}", p.display())
                        })
                    },
                ),
                Dialog::WorkspaceName { editing, name } => {
                    self.save_workspace_name(editing, name);
                }
                Dialog::Trash { paths } => self.start_described_job(
                    ctx,
                    crate::i18n::text("Удаление файлов"),
                    format!(
                        "{} ({}): {}",
                        crate::i18n::text("Удаление файлов"),
                        paths.len(),
                        paths.first().map(Location::display).unwrap_or_default()
                    ),
                    move |progress| {
                        fs_ops::trash_entries_with_progress(&paths, progress).map(|n| {
                            crate::tr_format!("Обработано объектов: {n}", "Items processed: {n}")
                        })
                    },
                ),
            }
        } else if !cancel {
            self.dialog = Some(dialog);
        }
        if self.dialog.is_none() && self.dialog_file_focus.is_some() {
            ctx.request_repaint();
        }
    }
}
