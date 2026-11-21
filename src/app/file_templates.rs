use super::{
    comparison::{
        show_progress_with_stop, ComparisonBody, ComparisonDialog, ComparisonResponse, Phase,
    },
    PawCommander,
};
use crate::{
    i18n::text,
    operations::TransferError,
    shortcuts::Command,
    vfs::{
        file_templates::{self, Library, NewFile, SavedTemplate, Source, TextEncoding},
        Location,
    },
};
use eframe::egui;
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[derive(Clone, Copy, PartialEq, Eq)]
enum BuiltIn {
    Empty,
    Markdown,
    Json,
    Csv,
}

impl BuiltIn {
    const ALL: [Self; 4] = [Self::Empty, Self::Markdown, Self::Json, Self::Csv];

    fn label(self) -> &'static str {
        match self {
            Self::Empty => text!("Пустой текстовый файл"),
            Self::Markdown => "Markdown",
            Self::Json => "JSON",
            Self::Csv => "CSV",
        }
    }

    fn filename(self) -> String {
        let extension = match self {
            Self::Empty => "txt",
            Self::Markdown => "md",
            Self::Json => "json",
            Self::Csv => "csv",
        };
        format!("{}.{extension}", text!("Без названия"))
    }

    fn contents(self) -> &'static str {
        match self {
            Self::Empty => "",
            Self::Markdown => "# Title\n",
            Self::Json => "{}\n",
            Self::Csv => "Name,Value\n",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Choice {
    BuiltIn(BuiltIn),
    Selected,
    Saved(usize),
}

struct Creation {
    target: Location,
    result: Receiver<Result<(), TransferError>>,
}

pub(super) struct FileTemplatesDialog {
    directory: Location,
    selection: Option<Location>,
    choice: Choice,
    filename: String,
    default_filename: String,
    body: String,
    encoding: TextEncoding,
    focus_name: bool,
    template_title: Option<String>,
    confirm_removal: bool,
    capture: Phase<SavedTemplate>,
    creating: Option<Creation>,
    notice: Option<(String, bool)>,
}

impl FileTemplatesDialog {
    fn new(directory: Location, selection: Option<Location>) -> Self {
        let default_filename = BuiltIn::Empty.filename();
        Self {
            directory,
            selection,
            choice: Choice::BuiltIn(BuiltIn::Empty),
            filename: default_filename.clone(),
            default_filename,
            body: String::new(),
            encoding: TextEncoding::default(),
            focus_name: true,
            template_title: None,
            confirm_removal: false,
            capture: Phase::Ready,
            creating: None,
            notice: None,
        }
    }

    pub(super) fn poll(&mut self) {
        self.capture.poll();
    }

    pub(super) fn show(
        &mut self,
        ctx: &egui::Context,
        library: &mut Library,
    ) -> ComparisonResponse {
        self.accept_capture(library);
        if let Some(target) = self.accept_creation() {
            return ComparisonResponse::RevealItem(target);
        }
        let saving = matches!(self.capture, Phase::Running(_));
        let busy = saving || self.creating.is_some();
        let mut close = false;
        let mut new_file = None;
        let modal = egui::Modal::new(egui::Id::unique("new-file")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 52.0).clamp(230.0, 610.0));
            ui.heading(Command::NewFile.label());
            let path = self.directory.display().escape_debug().to_string();
            ui.add(egui::Label::new(&path).truncate().selectable(true))
                .on_hover_text(path);
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("new-file-form")
                .max_height((ctx.content_rect().height() - 150.0).max(100.0))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        self.show_template(ui, library);
                        let label = text!("Имя файла");
                        ui.label(label);
                        let name = ui.add(
                            egui::TextEdit::singleline(&mut self.filename)
                                .desired_width(f32::INFINITY),
                        );
                        ui.ctx()
                            .accesskit_node_builder(name.id, |node| node.set_label(label));
                        if self.focus_name && !ui.is_sizing_pass() {
                            name.request_focus();
                            self.focus_name = false;
                        }
                        match self.choice {
                            Choice::BuiltIn(_) => self.show_body(ui),
                            Choice::Selected | Choice::Saved(_) => {
                                ui.small(text!(
                                    "Содержимое и кодировка исходного шаблона сохраняются.",
                                ));
                            }
                        }
                        self.show_template_actions(ui, library);
                    });
                    if let Some((message, error)) = &self.notice {
                        if *error {
                            ui.colored_label(ui.visuals().error_fg_color, message);
                        } else {
                            ui.label(message);
                        }
                    }
                });
            ui.add_space(6.0);
            if let Phase::Running(worker) = &mut self.capture {
                show_progress_with_stop(
                    ui,
                    worker,
                    crate::i18n::key!("Сохраняем шаблон…"),
                    crate::i18n::key!("Остановка…"),
                );
                if ui
                    .add_enabled(
                        !worker.progress.cancel.is_requested(),
                        egui::Button::new(text!("Остановить")),
                    )
                    .clicked()
                {
                    worker.progress.cancel.request();
                }
            } else if self.creating.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(text!("Создаём файл…"));
                    close = ui.button(text!("Закрыть")).clicked();
                });
            } else {
                ui.horizontal(|ui| {
                    let valid = file_templates::validate_filename(&self.filename).is_ok();
                    if ui
                        .add_enabled(valid, egui::Button::new(text!("Создать")))
                        .on_disabled_hover_text(text!("Введите имя файла без разделителей пути"))
                        .clicked()
                    {
                        let job = NewFile {
                            directory: self.directory.clone(),
                            name: self.filename.clone(),
                            source: self.source(library),
                        };
                        let (reply, result) = mpsc::channel();
                        self.creating = Some(Creation {
                            target: job.target(),
                            result,
                        });
                        self.notice = None;
                        new_file = Some(ComparisonResponse::CreateFile(job, reply));
                    }
                    close = ui.button(text!("Отмена")).clicked();
                });
            }
        });
        if let Some(request) = new_file {
            return request;
        }
        if close || modal.should_close() {
            if let Phase::Running(worker) = &self.capture {
                worker.progress.cancel.request();
            } else {
                return ComparisonResponse::Close;
            }
        }
        ComparisonResponse::Open
    }

    fn show_template(&mut self, ui: &mut egui::Ui, library: &Library) {
        let label = match self.choice {
            Choice::BuiltIn(kind) => kind.label(),
            Choice::Selected => text!("Выделенный файл"),
            Choice::Saved(index) => &library.templates[index].title,
        };
        let mut choice = self.choice;
        ui.horizontal_wrapped(|ui| {
            ui.label(text!("Шаблон файла"));
            egui::ComboBox::from_id_salt("file-template")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    for kind in BuiltIn::ALL {
                        ui.selectable_value(&mut choice, Choice::BuiltIn(kind), kind.label());
                    }
                    if let Some(file) = &self.selection {
                        ui.separator();
                        let name = file.name().escape_debug().to_string();
                        ui.selectable_value(
                            &mut choice,
                            Choice::Selected,
                            crate::tr_format!("Selected file · {name}", name = name),
                        );
                    }
                    if !library.templates.is_empty() {
                        ui.separator();
                        for (index, template) in library.templates.iter().enumerate() {
                            ui.selectable_value(&mut choice, Choice::Saved(index), &template.title);
                        }
                    }
                });
        });
        if choice != self.choice {
            self.choose(choice, library);
        }
    }

    fn show_body(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(text!("Кодировка"));
            egui::ComboBox::from_id_salt("new-file-encoding")
                .selected_text(self.encoding.label())
                .show_ui(ui, |ui| {
                    for encoding in TextEncoding::ALL {
                        ui.selectable_value(&mut self.encoding, encoding, encoding.label());
                    }
                });
        });
        let label = text!("Содержимое файла");
        ui.label(label);
        let field = ui.add(
            egui::TextEdit::multiline(&mut self.body)
                .desired_rows(5)
                .char_limit(file_templates::MAX_BYTES)
                .desired_width(f32::INFINITY),
        );
        ui.ctx()
            .accesskit_node_builder(field.id, |node| node.set_label(label));
    }

    fn show_template_actions(&mut self, ui: &mut egui::Ui, library: &mut Library) {
        if let Choice::Saved(index) = self.choice {
            if ui.button(text!("Удалить сохранённый шаблон")).clicked() {
                self.confirm_removal = true;
            }
            if self.confirm_removal {
                ui.horizontal_wrapped(|ui| {
                    ui.label(text!("Удалить этот шаблон?"));
                    if ui.button(text!("Удалить")).clicked() {
                        library.templates.remove(index);
                        self.choose(Choice::BuiltIn(BuiltIn::Empty), library);
                    }
                    if ui.button(text!("Отмена")).clicked() {
                        self.confirm_removal = false;
                    }
                });
            }
        } else {
            let valid = file_templates::validate_filename(&self.filename).is_ok();
            if ui
                .add_enabled(valid, egui::Button::new(text!("Сохранить как шаблон")))
                .clicked()
            {
                self.template_title = Some(self.filename.clone());
                self.notice = None;
            }
        }
        if let Some(title) = &mut self.template_title {
            let mut save = false;
            let mut cancel = false;
            ui.horizontal_wrapped(|ui| {
                let label = text!("Название шаблона");
                let field = ui.add(egui::TextEdit::singleline(title).hint_text(label));
                ui.ctx()
                    .accesskit_node_builder(field.id, |node| node.set_label(label));
                save = ui.button(text!("Сохранить")).clicked();
                cancel = ui.button(text!("Отмена")).clicked();
            });
            if cancel {
                self.template_title = None;
            } else if save {
                let title = title.trim().to_owned();
                if let Err(error) = library.check_title(&title) {
                    self.notice = Some((error, true));
                    return;
                }
                let filename = self.filename.clone();
                let source = self.source(library);
                self.notice = None;
                self.capture = Phase::start(ui.ctx(), move |progress| {
                    file_templates::save_template(title, filename, source, progress)
                });
            }
        }
    }

    fn choose(&mut self, choice: Choice, library: &Library) {
        let filename = match choice {
            Choice::BuiltIn(kind) => {
                self.body = kind.contents().into();
                kind.filename()
            }
            Choice::Selected => self.selection.as_ref().unwrap().name(),
            Choice::Saved(index) => library.templates[index].filename.clone(),
        };
        if self.filename == self.default_filename || self.filename.is_empty() {
            self.filename.clone_from(&filename);
        }
        self.default_filename = filename;
        self.choice = choice;
        self.template_title = None;
        self.confirm_removal = false;
        self.notice = None;
    }

    fn source(&self, library: &Library) -> Source {
        match self.choice {
            Choice::BuiltIn(_) => Source::Text {
                value: self.body.clone(),
                encoding: self.encoding,
            },
            Choice::Selected => Source::File(self.selection.as_ref().unwrap().clone()),
            Choice::Saved(index) => library.templates[index].source(),
        }
    }

    fn accept_capture(&mut self, library: &mut Library) {
        if matches!(self.capture, Phase::Ready | Phase::Running(_)) {
            return;
        }
        self.notice = Some(match std::mem::replace(&mut self.capture, Phase::Ready) {
            Phase::Finished(template) => match library.insert(template) {
                Ok(index) => {
                    self.choose(Choice::Saved(index), library);
                    (text!("Шаблон сохранён").into(), false)
                }
                Err(error) => (error, true),
            },
            Phase::Failed(error) => (error, true),
            Phase::Stopped => (text!("Операция остановлена").into(), false),
            Phase::Ready | Phase::Running(_) => unreachable!(),
        });
    }

    fn accept_creation(&mut self) -> Option<Location> {
        let pending = self.creating.as_ref()?;
        let notice = match pending.result.try_recv() {
            Ok(Ok(())) => return Some(self.creating.take().unwrap().target),
            Ok(Err(TransferError::Cancelled)) => (crate::tr_format!("Operation cancelled"), false),
            Ok(Err(error)) => (crate::i18n::file_error(&error), true),
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                let path = pending.target.display().escape_debug().to_string();
                (
                    crate::tr_format!(
                        "Could not confirm file creation at {path}. Check the destination folder.",
                        path = path
                    ),
                    true,
                )
            }
        };
        self.creating = None;
        self.notice = Some(notice);
        self.focus_name = true;
        None
    }
}

impl PawCommander {
    pub(super) fn can_create_file(&self) -> bool {
        !self.tab().path.is_archive() && !self.conflict_open()
    }

    pub(super) fn open_new_file(&mut self, return_focus: Option<egui::Id>, ctx: &egui::Context) {
        if self.comparison.is_some() || !self.can_create_file() {
            return;
        }
        let selected = self.selected();
        let selected = match selected.as_slice() {
            [path]
                if self
                    .tab()
                    .entries
                    .iter()
                    .any(|entry| entry.path == *path && !entry.is_dir && !entry.is_symlink) =>
            {
                Some(path.clone())
            }
            _ => None,
        };
        let dialog = FileTemplatesDialog::new(self.tab().path.clone(), selected);
        self.comparison = Some(ComparisonDialog::new(
            ComparisonBody::NewFile(Box::new(dialog)),
            return_focus,
            ctx,
        ));
    }
}
