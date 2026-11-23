use crate::i18n::text;
use crate::tools::custom_action::{Template, TemplateError};
use crate::tools::launch::Invocation;
use eframe::egui::{self, RichText};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub(super) struct Editor {
    templates: Vec<Template>,
    selected: usize,
}

impl Editor {
    pub(super) fn new(templates: Vec<Template>) -> Self {
        Self {
            templates,
            selected: 0,
        }
    }

    pub(super) fn can_save(&self) -> bool {
        self.templates
            .iter()
            .all(|template| template.validate().is_ok())
    }

    pub(super) fn into_templates(self) -> Vec<Template> {
        self.templates
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui, focus: bool) {
        let mut focus_name = false;
        ui.horizontal(|ui| {
            if !self.templates.is_empty() {
                choose_action(ui, &self.templates, &mut self.selected);
            }
            let add = ui.button(text!("Добавить действие"));
            if focus && !ui.is_sizing_pass() {
                add.request_focus();
            }
            if add.clicked() {
                self.templates.push(Template::default());
                self.selected = self.templates.len() - 1;
                focus_name = true;
            }
            if !self.templates.is_empty() && ui.button(text!("Удалить действие")).clicked()
            {
                self.templates.remove(self.selected);
                self.selected = self.selected.min(self.templates.len().saturating_sub(1));
            }
        });
        let Some(template) = self.templates.get_mut(self.selected) else {
            ui.label(text!("Добавьте действие для выбранных файлов."));
            return;
        };

        ui.push_id(self.selected, |ui| {
            let name = string_field(ui, crate::i18n::key!("Название действия"), &mut template.name);
            if focus_name {
                name.request_focus();
            }
            string_field(ui, crate::i18n::key!("Исполняемый файл"), &mut template.program);

            ui.add_space(4.0);
            ui.label(text!("Один аргумент в каждой строке"));
            ui.small(text!("{file} — один выбранный объект, {files} — всё выделение, {folder} — текущая папка. Для буквальных скобок используйте {{ и }}."));
            let mut remove = None;
            egui::ScrollArea::vertical()
                .id_salt("argument_fields")
                .max_height((ui.ctx().content_rect().height() - 340.0).clamp(112.0, 260.0))
                .show(ui, |ui| {
                    for (index, argument) in template.arguments.iter_mut().enumerate() {
                        ui.push_id(index, |ui| {
                            ui.horizontal(|ui| {
                                let label = ui.label(format!("{}", index + 1));
                                ui.add(egui::TextEdit::singleline(argument)
                                    .desired_width((ui.available_width() - 104.0).max(80.0)))
                                    .labelled_by(label.id);
                                if ui.button(text!("Удалить")).clicked() {
                                    remove = Some(index);
                                }
                            });
                        });
                    }
                });
            if let Some(index) = remove {
                template.arguments.remove(index);
            }
            if ui.button(text!("Добавить аргумент")).clicked() {
                template.arguments.push(String::new());
            }
        });
        if let Some((template, error)) = self
            .templates
            .iter()
            .find_map(|template| template.validate().err().map(|error| (template, error)))
        {
            let name = template_name(template);
            let error = crate::i18n::launch::template_error(&error);
            ui.colored_label(ui.visuals().error_fg_color, format!("{name} · {error}"));
        }
    }
}

fn string_field(
    ui: &mut egui::Ui,
    label: crate::i18n::MessageKey,
    value: &mut String,
) -> egui::Response {
    let label = ui.label(text(label));
    ui.add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY))
        .labelled_by(label.id)
}

fn template_name(template: &Template) -> &str {
    let name = template.name.trim();
    if name.is_empty() {
        text!("Без названия")
    } else {
        name
    }
}

fn choose_action(ui: &mut egui::Ui, templates: &[Template], selected: &mut usize) {
    let label = ui.label(text!("Действие"));
    egui::ComboBox::from_id_salt("custom_action_choice")
        .width((ui.available_width() - 230.0).clamp(135.0, 280.0))
        .selected_text(template_name(&templates[*selected]))
        .show_ui(ui, |ui| {
            for (index, template) in templates.iter().enumerate() {
                ui.selectable_value(selected, index, template_name(template));
            }
        })
        .response
        .labelled_by(label.id);
}

pub(super) struct Preview {
    folder: PathBuf,
    files: Vec<PathBuf>,
    templates: Vec<Template>,
    selected: usize,
    prepared: Result<Prepared, TemplateError>,
}

struct Prepared {
    invocation: Invocation,
    arguments: String,
}

impl Prepared {
    fn new(template: &Template, folder: &Path, files: &[PathBuf]) -> Result<Self, TemplateError> {
        let invocation = template.prepare(folder, files)?;
        let mut arguments = String::new();
        for (index, value) in invocation.arguments.iter().enumerate() {
            let _ = writeln!(arguments, "{}  {value:?}", index + 1);
        }
        Ok(Self {
            invocation,
            arguments,
        })
    }
}

impl Preview {
    pub(super) fn new(
        folder: PathBuf,
        files: Vec<PathBuf>,
        templates: Vec<Template>,
    ) -> Option<Self> {
        let prepared = Prepared::new(templates.first()?, &folder, &files);
        Some(Self {
            folder,
            files,
            templates,
            selected: 0,
            prepared,
        })
    }

    pub(super) fn can_run(&self) -> bool {
        self.prepared.is_ok()
    }

    pub(super) fn into_invocation(self) -> Option<Invocation> {
        self.prepared.ok().map(|prepared| prepared.invocation)
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui) {
        let before = self.selected;
        ui.horizontal(|ui| choose_action(ui, &self.templates, &mut self.selected));
        if self.selected != before {
            self.prepared =
                Prepared::new(&self.templates[self.selected], &self.folder, &self.files);
        }
        match &self.prepared {
            Ok(prepared) => {
                let invocation = &prepared.invocation;
                ui.label(text!("Исполняемый файл"));
                ui.monospace(format!("{:?}", invocation.details.program.as_os_str()));
                ui.label(text!("Рабочая папка"));
                ui.monospace(format!("{:?}", invocation.details.folder.as_os_str()));
                ui.label(crate::tr_format!(
                    "Arguments ({})",
                    invocation.arguments.len()
                ));
                egui::ScrollArea::vertical()
                    .id_salt("launch_arguments")
                    .max_height((ui.ctx().content_rect().height() - 300.0).clamp(96.0, 280.0))
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(&prepared.arguments).monospace())
                                .wrap()
                                .selectable(true),
                        );
                    });
            }
            Err(error) => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    crate::i18n::launch::template_error(error),
                );
            }
        }
    }
}
