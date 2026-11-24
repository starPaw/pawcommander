use super::{dialogs::Dialog, jobs::HistoryWarning, PawCommander};
use crate::i18n::{key, launch::editor_preparation_error, text, MessageKey};
use crate::operations::TransferError;
use crate::tools::launch::{CommandDetails, Invocation, Target};
use crate::tools::system_open::Mode;
use crate::vfs::{Location, RemoteEdit, RemoteEditSaveOutcome};
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
    fn check(&self) -> Result<(), MessageKey> {
        if !Path::new(&self.program).is_absolute() || self.program.contains('\0') {
            return Err(key!("Укажите абсолютный путь к исполняемому файлу"));
        }
        if self.arguments.contains('\0') {
            return Err(key!("Аргумент содержит нулевой байт."));
        }
        Ok(())
    }

    fn invocation(&self, source: &Location, file: &Path) -> Result<Invocation, MessageKey> {
        self.check()?;
        let folder = file.parent().ok_or(key!("Недопустимый путь"))?;
        let arguments = self
            .arguments
            .lines()
            .map(Into::into)
            .chain([file.as_os_str().to_owned()])
            .collect();
        Ok(Invocation {
            details: CommandDetails {
                target: Target::Editor {
                    source: source.clone(),
                    file: file.to_owned(),
                },
                program: self.program.as_str().into(),
                folder: folder.to_owned(),
            },
            arguments,
        })
    }
}

impl Editors {
    pub(super) fn valid(&self) -> bool {
        self.local.check().is_ok() && self.sftp.check().is_ok()
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui) {
        ui.small(text!(
            "Укажите путь к программе и по одному аргументу на строке. Путь к файлу добавляется последним.",
        ));
        for (label, command) in [
            (crate::i18n::key!("Локальные файлы"), &mut self.local),
            (crate::i18n::key!("Рабочие копии SFTP"), &mut self.sftp),
        ] {
            ui.push_id(label, |ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(text(label)).strong());
                let program = ui.label(text!("Исполняемый файл"));
                ui.add(
                    egui::TextEdit::singleline(&mut command.program).desired_width(f32::INFINITY),
                )
                .labelled_by(program.id);
                let args = ui.label(text!("Аргументы"));
                ui.add(
                    egui::TextEdit::multiline(&mut command.arguments)
                        .desired_width(f32::INFINITY)
                        .desired_rows(2),
                )
                .labelled_by(args.id);
                if let Err(error) = command.check() {
                    ui.colored_label(ui.visuals().error_fg_color, text(error));
                }
            });
        }
    }
}

#[derive(Default)]
pub(super) struct Session {
    edit: Option<RemoteEdit>,
    download: Option<Receiver<Downloaded>>,
    saving: bool,
    feedback: Option<SaveFeedback>,
    open: bool,
}

struct Downloaded(RemoteEdit, EditorCommand);

pub(super) type SaveResult = Result<RemoteEditSaveOutcome, TransferError>;

struct SaveFeedback {
    result: SaveResult,
    history_warning: Option<HistoryWarning>,
}

impl Session {
    pub(super) fn available(&self) -> bool {
        self.edit.is_some()
    }

    pub(super) fn reveal(&mut self) {
        self.open = true;
    }

    pub(super) fn finish_save(
        &mut self,
        edit: Option<RemoteEdit>,
        result: SaveResult,
        history_warning: Option<HistoryWarning>,
    ) {
        if let Some(edit) = edit {
            self.edit = Some(edit);
        }
        self.saving = false;
        self.feedback = Some(SaveFeedback {
            result,
            history_warning,
        });
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
        if let Some(file) = path.local() {
            self.launch_editor(
                self.settings.external_editors.local.clone(),
                &path,
                file,
                ctx,
            );
            return;
        }
        if !path.remote() {
            return;
        }
        if let Some(edit) = &self.remote_editor.edit {
            if edit.target() != &path {
                self.notice = Some((
                    text!("Завершите текущую правку SFTP, чтобы открыть другую.").into(),
                    false,
                ));
            }
            self.remote_editor.reveal();
            return;
        }
        if self.remote_editor.download.is_some() {
            return;
        }
        let command = self.settings.external_editors.sftp.clone();
        if let Err(error) = command.check() {
            self.notice = Some((
                editor_preparation_error(&path, Path::new(&command.program), text(error)),
                true,
            ));
            self.configure_external_editors(ctx);
            return;
        }
        let label = text!("Подготовка рабочей копии SFTP");
        let (send, receive) = mpsc::channel();
        self.remote_editor.download = Some(receive);
        self.start_typed_stoppable_job(ctx, label, format!("{label} · {path}"), move |progress| {
            let copies = dirs::data_local_dir()
                .ok_or_else(|| text!("Не найдена папка данных приложения."))?
                .join("PawCommander/Editor Copies");
            let edit = RemoteEdit::download(path, &copies, progress)?;
            let _ = send.send(Downloaded(edit, command));
            Ok(text!("Рабочая копия готова.").into())
        });
    }

    fn launch_editor(
        &mut self,
        command: EditorCommand,
        source: &Location,
        file: &Path,
        ctx: &egui::Context,
    ) {
        let invocation = match command.invocation(source, file) {
            Ok(invocation) => invocation,
            Err(error) => {
                self.notice = Some((
                    editor_preparation_error(source, Path::new(&command.program), text(error)),
                    true,
                ));
                return;
            }
        };
        self.launch_configured(ctx, text!("Открытие внешнего редактора…"), invocation);
    }

    pub(super) fn poll_external_editors(&mut self, ctx: &egui::Context) {
        if self.application_job.is_some() {
            return;
        }
        let Some(download) = self.remote_editor.download.as_ref() else {
            return;
        };
        let result = match download.try_recv() {
            Err(TryRecvError::Empty) => return,
            other => other,
        };
        self.remote_editor.download = None;
        if let Ok(Downloaded(edit, command)) = result {
            self.launch_editor(command, edit.target(), edit.copy(), ctx);
            self.remote_editor.edit = Some(edit);
            self.remote_editor.feedback = None;
            self.remote_editor.open = true;
        }
    }

    fn save_remote_edit(&mut self, edit: RemoteEdit, ctx: &egui::Context) {
        self.remote_editor.saving = true;
        self.remote_editor.feedback = None;
        self.notice = None;
        self.enqueue_remote_save(edit, ctx);
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
        egui::Window::new(text!("Правка SFTP"))
            .id(egui::Id::unique("remote_editor"))
            .open(&mut session.open)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.add(
                    egui::Label::new(edit.target().display())
                        .wrap()
                        .selectable(true),
                );
                ui.small(text!(
                    "Сохраните файл в редакторе, затем отправьте правку на сервер.",
                ));
                if let Some(feedback) = &session.feedback {
                    let notice = save_notice(edit.target(), edit.copy(), &feedback.result);
                    let message = match notice {
                        Ok(message) if has_cleanup_warning(&feedback.result) => {
                            egui::RichText::new(message).color(ui.visuals().warn_fg_color)
                        }
                        Ok(message) => egui::RichText::new(message),
                        Err(error) => egui::RichText::new(error).color(ui.visuals().error_fg_color),
                    };
                    ui.add(egui::Label::new(message).wrap().selectable(true));
                    if let Some(warning) = &feedback.history_warning {
                        let warning =
                            egui::RichText::new(warning.notice()).color(ui.visuals().warn_fg_color);
                        ui.add(egui::Label::new(warning).wrap().selectable(true));
                    }
                }
                ui.horizontal_wrapped(|ui| {
                    let available = !session.saving;
                    save = ui
                        .add_enabled(available, egui::Button::new(text!("Сохранить на сервере")))
                        .clicked();
                    reopen = ui.button(text!("Открыть рабочую копию")).clicked();
                    reveal = ui.button(text!("Показать в Finder")).clicked();
                    finish = ui
                        .add_enabled(available, egui::Button::new(text!("Завершить сеанс")))
                        .clicked();
                });
                ui.small(text!(
                    "После завершения сеанса рабочая копия остаётся на Mac.",
                ));
            });
        if save {
            let edit = edit.clone();
            self.save_remote_edit(edit, ctx);
        } else if reopen {
            let source = edit.target().clone();
            let copy = edit.copy().to_owned();
            self.launch_editor(
                self.settings.external_editors.sftp.clone(),
                &source,
                &copy,
                ctx,
            );
        } else if reveal {
            let source = edit.target().clone();
            let copy = edit.copy().to_owned();
            self.launch_system_open(Mode::RevealWorkingCopy, copy, Some(source), ctx);
        } else if finish {
            self.remote_editor = Session::default();
        }
    }
}

fn has_cleanup_warning(result: &SaveResult) -> bool {
    matches!(
        result,
        Ok(RemoteEditSaveOutcome::Saved {
            cleanup_warning: Some(_)
        })
    )
}

pub(super) fn save_notice(
    target: &Location,
    copy: &Path,
    result: &SaveResult,
) -> Result<String, String> {
    use crate::i18n::{display::launch_location, file_error};

    let saved = text!("Правка сохранена на сервере.");
    let details = |message: &str| {
        let destination =
            crate::tr_format!("SFTP destination · {path}", path = launch_location(target));
        let copy = crate::tr_format!("Working copy · {path}", path = format_args!("{copy:?}"));
        format!("{message}\n{destination}\n{copy}")
    };
    match result {
        Ok(RemoteEditSaveOutcome::AlreadyCurrent) => {
            Ok(text!("На сервере уже сохранена эта версия.").into())
        }
        Ok(RemoteEditSaveOutcome::Saved {
            cleanup_warning: None,
        }) => Ok(saved.into()),
        Ok(RemoteEditSaveOutcome::Saved {
            cleanup_warning: Some(warning),
        }) => {
            let warning =
                crate::tr_format!("Cleanup warning: {error}", error = file_error(warning));
            Ok(details(&format!("{saved}\n{warning}")))
        }
        Err(TransferError::Cancelled) => Ok(crate::tr_format!("Operation cancelled")),
        Err(error @ TransferError::PublicationUnconfirmed(_)) => Err(details(&crate::tr_format!(
            "Could not confirm the SFTP save. {error}",
            error = file_error(error)
        ))),
        Err(error) => Err(details(&file_error(error))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_error::Failure;

    #[test]
    fn editor_save_notice_keeps_publication_warning_distinct_from_failed_or_stopped_save() {
        let target = Location::Local("/fixture/server 'edit'.txt".into());
        let copy = Path::new("/fixture/local copy.txt");
        let warning = Ok(RemoteEditSaveOutcome::Saved {
            cleanup_warning: Some("retained backup at /fixture/staging".into()),
        });
        let message = save_notice(&target, copy, &warning).unwrap();
        assert!(has_cleanup_warning(&warning));
        assert!(message.starts_with(text!("Правка сохранена на сервере.")));
        assert!(message.contains("retained backup at /fixture/staging"));
        assert!(message.contains("/fixture/server"));
        assert!(message.contains("/fixture/local copy.txt"));

        let unconfirmed = Err(TransferError::PublicationUnconfirmed(Failure::Message(
            "server disconnected after rename".into(),
        )));
        let message = save_notice(&target, copy, &unconfirmed).unwrap_err();
        assert!(message.contains(&crate::tr_format!(
            "Could not confirm the SFTP save. {error}",
            error = "server disconnected after rename"
        )));
        assert!(message.contains("/fixture/local copy.txt"));
        assert!(!has_cleanup_warning(&unconfirmed));

        let cancelled = Err(TransferError::Cancelled);
        assert_eq!(
            save_notice(&target, copy, &cancelled),
            Ok(crate::tr_format!("Operation cancelled"))
        );
        assert!(!has_cleanup_warning(&cancelled));
    }

    #[test]
    fn editor_session_keeps_history_warning_when_the_window_reopens() {
        use crate::result_history::{HistoryError, HistorySaveProblem};
        use std::{io, sync::Arc};

        let mut session = Session {
            saving: true,
            ..Session::default()
        };
        let cause = Arc::new(HistoryError::Save {
            path: "/fixture/history.json".into(),
            problem: HistorySaveProblem::Replace(io::Error::from_raw_os_error(libc::EIO)),
        });
        session.finish_save(
            None,
            Ok(RemoteEditSaveOutcome::Saved {
                cleanup_warning: None,
            }),
            Some(HistoryWarning::Save {
                cause: cause.clone(),
                recovery_path: "/fixture/recovery".into(),
            }),
        );
        session.reveal();
        let feedback = session.feedback.unwrap();
        assert!(session.open && !session.saving);
        assert!(matches!(
            feedback.result,
            Ok(RemoteEditSaveOutcome::Saved {
                cleanup_warning: None
            })
        ));
        assert!(matches!(
            feedback.history_warning,
            Some(HistoryWarning::Save { cause: received, recovery_path })
                if Arc::ptr_eq(&received, &cause) && recovery_path == Path::new("/fixture/recovery")
        ));
    }
}
