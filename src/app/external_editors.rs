use super::{dialogs::Dialog, PawCommander};
use crate::i18n::text;
use crate::tools::custom_action::Invocation;
use crate::vfs::{Location, RemoteEdit};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct Editors {
    local: EditorCommand,
    sftp: EditorCommand,
}

#[derive(Clone, Deserialize, Serialize)]
struct EditorCommand {
    program: String,
    arguments: String,
}

impl Default for EditorCommand {
    fn default() -> Self {
        Self {
            program: "/usr/bin/open".into(),
            arguments: "-t".into(),
        }
    }
}

impl EditorCommand {
    fn check(&self) -> Result<(), &'static str> {
        if !Path::new(&self.program).is_absolute() || self.program.contains('\0') {
            return Err(text("Укажите абсолютный путь к исполняемому файлу"));
        }
        if self.arguments.contains('\0') {
            return Err(text("Аргумент содержит нулевой байт."));
        }
        Ok(())
    }

    fn invocation(self, file: &Path) -> Result<Invocation, String> {
        self.check()?;
        let folder = file.parent().ok_or_else(|| text("Недопустимый путь"))?;
        let arguments = self
            .arguments
            .lines()
            .map(Into::into)
            .chain([file.as_os_str().to_owned()])
            .collect();
        Ok(Invocation {
            name: text("Внешний редактор").into(),
            program: self.program.into(),
            folder: folder.to_owned(),
            arguments,
        })
    }
}

impl Editors {
    pub(super) fn valid(&self) -> bool {
        self.local.check().is_ok() && self.sftp.check().is_ok()
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui) {
        ui.small(text(
            "Укажите путь к программе и по одному аргументу на строке. Путь к файлу добавляется последним.",
        ));
        for (label, command) in [
            ("Локальные файлы", &mut self.local),
            ("Рабочие копии SFTP", &mut self.sftp),
        ] {
            ui.push_id(label, |ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(text(label)).strong());
                let program = ui.label(text("Исполняемый файл"));
                ui.add(
                    egui::TextEdit::singleline(&mut command.program).desired_width(f32::INFINITY),
                )
                .labelled_by(program.id);
                let args = ui.label(text("Аргументы"));
                ui.add(
                    egui::TextEdit::multiline(&mut command.arguments)
                        .desired_width(f32::INFINITY)
                        .desired_rows(2),
                )
                .labelled_by(args.id);
                if let Err(error) = command.check() {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
            });
        }
    }
}

#[derive(Default)]
pub(super) struct Session {
    edit: Option<RemoteEdit>,
    work: Option<Receiver<Finished>>,
    feedback: Option<Result<String, String>>,
    open: bool,
}

enum Finished {
    Downloaded(RemoteEdit, EditorCommand),
    Saved(RemoteEdit, Result<String, String>),
}

impl Session {
    pub(super) fn available(&self) -> bool {
        self.edit.is_some()
    }

    pub(super) fn reveal(&mut self) {
        self.open = true;
    }
}

impl PawCommander {
    pub(super) fn configure_external_editors(&mut self, ctx: &egui::Context) {
        self.remember_dialog_file_focus(ctx);
        self.dialog = Some(Dialog::ExternalEditors(Box::new(
            self.settings.external_editors.clone(),
        )));
        self.dialog_focus = true;
    }

    pub(super) fn open_external_editor(&mut self, path: Location, ctx: &egui::Context) {
        if let Some(path) = path.local() {
            self.launch_editor(self.settings.external_editors.local.clone(), path, ctx);
            return;
        }
        if !path.remote() {
            return;
        }
        if let Some(edit) = &self.remote_editor.edit {
            if edit.target() != &path {
                self.notice = Some((
                    text("Завершите текущую правку SFTP, чтобы открыть другую.").into(),
                    false,
                ));
            }
            self.remote_editor.reveal();
            return;
        }
        if self.remote_editor.work.is_some() {
            return;
        }
        let command = self.settings.external_editors.sftp.clone();
        if let Err(error) = command.check() {
            self.notice = Some((error.into(), true));
            self.configure_external_editors(ctx);
            return;
        }
        let label = text("Подготовка рабочей копии SFTP");
        let (send, receive) = mpsc::channel();
        self.remote_editor.work = Some(receive);
        self.start_stoppable_job(ctx, label, format!("{label} · {path}"), move |progress| {
            let copies = dirs::data_local_dir()
                .ok_or_else(|| text("Не найдена папка данных приложения."))?
                .join("PawCommander/Editor Copies");
            let edit = RemoteEdit::download(path, &copies, progress).map_err(|e| e.to_string())?;
            let _ = send.send(Finished::Downloaded(edit, command));
            Ok(text("Рабочая копия готова.").into())
        });
    }

    fn launch_editor(&mut self, command: EditorCommand, file: &Path, ctx: &egui::Context) {
        let invocation = match command.invocation(file) {
            Ok(invocation) => invocation,
            Err(error) => {
                self.notice = Some((error, true));
                return;
            }
        };
        let path = file.to_owned();
        self.launch_application(
            ctx,
            text("Открытие внешнего редактора…"),
            move |report| {
                if path.is_file() {
                    invocation.launch(report);
                } else {
                    report(Err(text("Файл для редактирования недоступен.").into()));
                }
            },
        );
    }

    pub(super) fn poll_external_editors(&mut self, ctx: &egui::Context) {
        if self.application_job.is_some() {
            return;
        }
        let Some(work) = self.remote_editor.work.as_ref() else {
            return;
        };
        let result = match work.try_recv() {
            Err(TryRecvError::Empty) => return,
            other => other,
        };
        self.remote_editor.work = None;
        match result {
            Ok(Finished::Downloaded(edit, command)) => {
                self.launch_editor(command, edit.copy(), ctx);
                self.remote_editor.edit = Some(edit);
                self.remote_editor.feedback = None;
                self.remote_editor.open = true;
            }
            Ok(Finished::Saved(edit, result)) => {
                self.remote_editor.edit = Some(edit);
                self.remote_editor.feedback = Some(result);
            }
            Err(_) => {}
        }
    }

    fn save_remote_edit(&mut self, mut edit: RemoteEdit, ctx: &egui::Context) {
        let label = text("Сохранение правки SFTP");
        let (send, receive) = mpsc::channel();
        self.remote_editor.work = Some(receive);
        self.remote_editor.feedback = None;
        self.notice = None;
        self.start_stoppable_job(
            ctx,
            label,
            format!("{label} · {}", edit.target()),
            move |progress| {
                let result = edit.save(progress);
                let _ = send.send(Finished::Saved(edit, result.clone()));
                result
            },
        );
    }

    pub(super) fn show_remote_editor(&mut self, ctx: &egui::Context) {
        let session = &mut self.remote_editor;
        let Some(edit) = &session.edit else {
            return;
        };
        let mut save = false;
        let mut reopen = false;
        let mut reveal = false;
        let mut finish = false;
        egui::Window::new(text("Правка SFTP"))
            .id(egui::Id::unique("remote_editor"))
            .open(&mut session.open)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.add(
                    egui::Label::new(edit.target().display())
                        .wrap()
                        .selectable(true),
                );
                ui.small(text(
                    "Сохраните файл в редакторе, затем отправьте правку на сервер.",
                ));
                if let Some(result) = &session.feedback {
                    match result {
                        Ok(message) => {
                            ui.label(message);
                        }
                        Err(error) => {
                            ui.colored_label(ui.visuals().error_fg_color, error);
                        }
                    }
                }
                ui.horizontal_wrapped(|ui| {
                    let available = session.work.is_none();
                    save = ui
                        .add_enabled(available, egui::Button::new(text("Сохранить на сервере")))
                        .clicked();
                    reopen = ui.button(text("Открыть рабочую копию")).clicked();
                    reveal = ui.button(text("Показать в Finder")).clicked();
                    finish = ui
                        .add_enabled(available, egui::Button::new(text("Завершить сеанс")))
                        .clicked();
                });
                ui.small(text(
                    "После завершения сеанса рабочая копия остаётся на Mac.",
                ));
            });
        if save {
            let edit = edit.clone();
            self.save_remote_edit(edit, ctx);
        } else if reopen {
            let copy = edit.copy().to_owned();
            self.launch_editor(self.settings.external_editors.sftp.clone(), &copy, ctx);
        } else if reveal {
            let copy = edit.copy().to_owned();
            self.launch_editor(
                EditorCommand {
                    program: "/usr/bin/open".into(),
                    arguments: "-R".into(),
                },
                &copy,
                ctx,
            );
        } else if finish {
            self.remote_editor = Session::default();
        }
    }
}
