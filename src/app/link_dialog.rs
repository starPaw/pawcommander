use super::PawCommander;
use crate::fs_ops::{links, validate_name};
use crate::{i18n::text, vfs::Location};
use eframe::egui;
use std::path::{Path, PathBuf};

pub(super) struct LinkDialog {
    directory: PathBuf,
    name: String,
    target: String,
    kind: links::Kind,
}

impl LinkDialog {
    pub(super) fn new(directory: PathBuf, target: String) -> Self {
        Self {
            directory,
            target,
            name: String::new(),
            kind: links::Kind::Symbolic,
        }
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui, focus_name: bool) {
        ui.weak(self.directory.display().to_string());
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(
                &mut self.kind,
                links::Kind::Symbolic,
                text("Символическая ссылка"),
            );
            ui.radio_value(&mut self.kind, links::Kind::Hard, text("Жёсткая ссылка"));
        });
        let name = ui.label(text("Имя ссылки"));
        let name_edit = ui
            .add(egui::TextEdit::singleline(&mut self.name).desired_width(f32::INFINITY))
            .labelled_by(name.id);
        if focus_name {
            name_edit.request_focus();
        }
        let target = ui.label(text("Путь к цели ссылки"));
        ui.add(egui::TextEdit::singleline(&mut self.target).desired_width(f32::INFINITY))
            .labelled_by(target.id);
        ui.small(text(
            "Относительный путь отсчитывается от папки, где создаётся ссылка.",
        ));
        if self.kind == links::Kind::Hard {
            ui.small(text(
                "Для жёсткой ссылки выберите обычный файл на том же томе.",
            ));
        }
    }

    pub(super) fn valid(&self) -> bool {
        validate_name(&self.name).is_ok() && !self.target.is_empty() && !self.target.contains('\0')
    }

    pub(super) fn submit(self, app: &mut PawCommander, ctx: &egui::Context) {
        let target = self.directory.join(&self.name);
        let label = text("Создание ссылки");
        let description = format!("{label} · {} → {}", target.display(), self.target);
        app.start_described_job(ctx, label, description, move |progress| {
            let path = Location::Local(target);
            progress.results.plan([(path.clone(), None)]);
            progress.results.begin(&path);
            progress.item(path.path());
            links::create(
                &self.directory,
                &self.name,
                Path::new(&self.target),
                self.kind,
                progress,
            )
        });
    }
}
