use super::{
    comparison::{
        show_progress_with_stop, ComparisonBody, ComparisonDialog, ComparisonResponse, Phase,
    },
    PawCommander,
};
use crate::{
    i18n::text,
    operations::{Progress, TransferError},
    shortcuts::Command,
    vfs::{
        collections::{self, Catalog, Collection, Unavailable},
        Location,
    },
};
use eframe::egui::{self, RichText};

struct Update {
    group: Collection,
    states: Vec<Option<Unavailable>>,
}

enum ResultView {
    Updated(Update, Option<crate::i18n::MessageKey>),
    Imported(Update),
    Exported(Location),
    Reveal(Location),
}

enum NameEdit {
    Create,
    Rename,
}

pub(super) struct CollectionsDialog {
    root: Option<Location>,
    selection: Vec<Location>,
    active: Option<usize>,
    selected: Option<usize>,
    states: Vec<Option<Unavailable>>,
    work: Phase<ResultView>,
    edit_name: Option<NameEdit>,
    name: String,
    remove_group: bool,
    filter: String,
    tags: String,
    add_tags: String,
    export_name: String,
    notice: Option<(String, bool)>,
}

impl CollectionsDialog {
    fn new(root: Option<Location>, selection: Vec<Location>) -> Self {
        Self {
            root,
            selection,
            active: None,
            selected: None,
            states: Vec::new(),
            work: Phase::Ready,
            edit_name: None,
            name: String::new(),
            remove_group: false,
            filter: String::new(),
            tags: String::new(),
            add_tags: String::new(),
            export_name: format!("collection{}", collections::PORTABLE_SUFFIX),
            notice: None,
        }
    }

    pub(super) fn poll(&mut self) {
        self.work.poll();
    }

    pub(super) fn show(
        &mut self,
        ctx: &egui::Context,
        catalog: &mut Catalog,
    ) -> ComparisonResponse {
        if let Some(path) = self.accept_result(catalog) {
            return ComparisonResponse::RevealItem(path);
        }
        let running = matches!(self.work, Phase::Running(_));
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::unique("collections")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 44.0).clamp(250.0, 720.0));
            ui.heading(Command::Collections.label());
            egui::ScrollArea::vertical()
                .id_salt("collection-body")
                .max_height((ctx.content_rect().height() - 132.0).max(112.0))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if let Some((message, error)) = &self.notice {
                        if *error {
                            ui.colored_label(ui.visuals().error_fg_color, message);
                        } else {
                            ui.label(message);
                        }
                    }
                    ui.add_enabled_ui(!running, |ui| {
                        self.show_groups(ui, catalog);
                        if let Some(index) = self.active {
                            self.show_members(ui, catalog, index);
                        }
                        self.show_portable(ui, catalog);
                    });
                });
            ui.add_space(4.0);
            if let Phase::Running(worker) = &mut self.work {
                show_progress_with_stop(
                    ui,
                    worker,
                    crate::i18n::key!("Работа с коллекцией…"),
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
            } else {
                close = ui.button(text!("Закрыть")).clicked();
            }
        });
        if modal.should_close() || close {
            if let Phase::Running(worker) = &self.work {
                worker.progress.cancel.request();
            } else {
                return ComparisonResponse::Close;
            }
        }
        ComparisonResponse::Open
    }

    fn show_groups(&mut self, ui: &mut egui::Ui, catalog: &mut Catalog) {
        let mut active = self.active;
        ui.horizontal_wrapped(|ui| {
            if !catalog.collections.is_empty() {
                let title = active.map_or("", |index| catalog.collections[index].name.as_str());
                egui::ComboBox::from_id_salt("collection-picker")
                    .selected_text(title)
                    .show_ui(ui, |ui| {
                        for (index, collection) in catalog.collections.iter().enumerate() {
                            ui.selectable_value(&mut active, Some(index), &collection.name);
                        }
                    });
            }
            if ui.button(text!("Новая коллекция")).clicked() {
                self.edit_name = Some(NameEdit::Create);
                self.name.clear();
                self.remove_group = false;
            }
            if let Some(index) = self.active {
                if ui.button(text!("Переименовать")).clicked() {
                    self.edit_name = Some(NameEdit::Rename);
                    self.name.clone_from(&catalog.collections[index].name);
                    self.remove_group = false;
                }
                if ui.button(text!("Удалить коллекцию")).clicked() {
                    self.remove_group = true;
                    self.edit_name = None;
                }
            }
        });
        if active != self.active {
            self.choose_group(active, catalog, ui.ctx());
        }
        if let Some(edit) = &self.edit_name {
            let create = matches!(edit, NameEdit::Create);
            ui.horizontal(|ui| {
                let label = text!("Название коллекции");
                let field = ui.add(egui::TextEdit::singleline(&mut self.name).hint_text(label));
                field.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
                if ui
                    .button(if create {
                        text!("Создать")
                    } else {
                        text!("Сохранить")
                    })
                    .clicked()
                {
                    let result = if create {
                        catalog.create(&self.name).map(Some)
                    } else if let Some(index) = self.active {
                        catalog.rename(index, &self.name).map(|()| Some(index))
                    } else {
                        return;
                    };
                    match result {
                        Ok(index) => self.choose_group(index, catalog, ui.ctx()),
                        Err(error) => self.notice = Some((error, true)),
                    }
                }
                if ui.button(text!("Отмена")).clicked() {
                    self.edit_name = None;
                }
            });
        }
        if self.remove_group
            && let Some(index) = self.active
        {
            ui.horizontal_wrapped(|ui| {
                let name = &catalog.collections[index].name;
                ui.label(crate::tr_format!(
                    "Remove collection “{name}”?",
                    name = name
                ));
                if ui.button(text!("Удалить коллекцию")).clicked() {
                    catalog.collections.remove(index);
                    let next = catalog
                        .collections
                        .len()
                        .checked_sub(1)
                        .map(|last| index.min(last));
                    self.choose_group(next, catalog, ui.ctx());
                }
                if ui.button(text!("Отмена")).clicked() {
                    self.remove_group = false;
                }
            });
        }
    }

    fn show_members(&mut self, ui: &mut egui::Ui, catalog: &mut Catalog, index: usize) {
        let capacity = catalog.capacity_for(index);
        let group = &mut catalog.collections[index];
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            let count = self.selection.len();
            if count != 0 {
                let label = text!("Теги через запятую");
                ui.add(
                    egui::TextEdit::singleline(&mut self.add_tags)
                        .hint_text(label)
                        .desired_width(180.0),
                )
                .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
                if ui
                    .button(crate::tr_format!("Add selection ({count})", count = count))
                    .clicked()
                {
                    match collections::parse_tags(&self.add_tags) {
                        Ok(tags) => {
                            let mut group = group.clone();
                            let paths = self.selection.clone();
                            self.start(ui.ctx(), move |progress| {
                                group.add(&paths, &tags, capacity, progress)?;
                                update(
                                    group,
                                    Some(crate::i18n::key!("Объекты добавлены")),
                                    progress,
                                )
                            });
                        }
                        Err(error) => self.notice = Some((error, true)),
                    }
                }
            }
            if !group.members.is_empty() && ui.button(text!("Обновить")).clicked() {
                self.refresh(group, ui.ctx());
            }
        });
        if group.members.is_empty() {
            ui.label(text!("В коллекции пока нет объектов"));
            return;
        }
        let label = text!("Фильтр по тегу");
        ui.add(
            egui::TextEdit::singleline(&mut self.filter)
                .hint_text(label)
                .desired_width(200.0),
        )
        .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
        self.show_rows(ui, group, index);
        self.show_member_editor(ui, group);
    }

    fn show_rows(&mut self, ui: &mut egui::Ui, group: &Collection, index: usize) {
        let filter = self.filter.trim().to_lowercase();
        let rows: Vec<_> = group
            .members
            .iter()
            .enumerate()
            .filter(|(_, member)| {
                filter.is_empty()
                    || member
                        .tags
                        .iter()
                        .any(|tag| tag.to_lowercase().contains(&filter))
            })
            .map(|(i, _)| i)
            .collect();
        if self
            .selected
            .is_some_and(|selected| !rows.contains(&selected))
        {
            self.selected = None;
        }
        if rows.is_empty() {
            ui.label(text!("Нет объектов с таким тегом"));
            return;
        }
        let height = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical()
            .id_salt(("collection-members", index))
            .max_height((ui.ctx().content_rect().height() * 0.30).clamp(84.0, 250.0))
            .auto_shrink([false, true])
            .show_rows(ui, height, rows.len(), |ui, range| {
                for &row in &rows[range] {
                    let member = &group.members[row];
                    let path = member.last_path().unwrap_or_default();
                    let name = path
                        .file_name()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy()
                        .escape_debug()
                        .to_string();
                    let tags = member.tags.join(", ");
                    let problem = self
                        .states
                        .get(row)
                        .and_then(Option::as_ref)
                        .map(Unavailable::message);
                    let line = if tags.is_empty() {
                        name
                    } else {
                        format!("{name}  ·  {tags}")
                    };
                    let line = match problem {
                        None => RichText::new(line),
                        Some(message) => {
                            RichText::new(format!("{line}  ·  {}", message.escape_debug()))
                                .color(ui.visuals().warn_fg_color)
                        }
                    };
                    let response = ui
                        .add(egui::Button::selectable(self.selected == Some(row), line).truncate());
                    let tooltip = path.display().to_string().escape_debug().to_string();
                    let tooltip = match problem {
                        Some(message) => format!("{tooltip}\n{message}"),
                        None => tooltip,
                    };
                    if response.on_hover_text(tooltip).clicked() {
                        self.selected = Some(row);
                        self.tags = tags;
                    }
                }
            });
    }

    fn show_member_editor(&mut self, ui: &mut egui::Ui, group: &mut Collection) {
        let Some(row) = self.selected else { return };
        let member = &group.members[row];
        let path = member.last_path().unwrap_or_default();
        let path = path.display().to_string().escape_debug().to_string();
        ui.add(egui::Label::new(&path).truncate().selectable(true))
            .on_hover_text(&path);
        let unavailable = self.states.get(row).and_then(Option::as_ref);
        if let Some(state) = unavailable {
            ui.colored_label(ui.visuals().warn_fg_color, state.message());
        }
        let can_relink = unavailable.is_some();
        let can_open = self.states.get(row).is_some_and(Option::is_none);
        ui.horizontal_wrapped(|ui| {
            let label = text!("Теги через запятую");
            ui.add(
                egui::TextEdit::singleline(&mut self.tags)
                    .hint_text(label)
                    .desired_width(190.0),
            )
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
            if ui.button(text!("Сохранить теги")).clicked() {
                match collections::parse_tags(&self.tags) {
                    Ok(tags) => {
                        self.tags = tags.join(", ");
                        group.members[row].tags = tags;
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some((error, true)),
                }
            }
            if ui
                .add_enabled(can_open, egui::Button::new(text!("Показать в панели")))
                .clicked()
            {
                let mut member = group.members[row].clone();
                self.start(ui.ctx(), move |_| {
                    let path = member
                        .resolve()
                        .map_err(|state| state.message().to_owned())?;
                    Ok(ResultView::Reveal(Location::Local(path)))
                });
            }
            if can_relink
                && let [path] = self.selection.as_slice()
                && ui.button(text!("Связать с выделенным")).clicked()
            {
                let path = path.clone();
                let mut group = group.clone();
                self.start(ui.ctx(), move |progress| {
                    if group.members[row].resolve().is_ok() {
                        return Err(text!("Объект снова доступен. Обновите коллекцию.").into());
                    }
                    group.relink(row, &path)?;
                    update(group, Some(crate::i18n::key!("Связь обновлена")), progress)
                });
            }
            if ui.button(text!("Убрать из коллекции")).clicked() {
                group.members.remove(row);
                if row < self.states.len() {
                    self.states.remove(row);
                }
                self.selected = None;
                self.notice = None;
            }
        });
    }

    fn show_portable(&mut self, ui: &mut egui::Ui, catalog: &Catalog) {
        let Some(root) = self.root.clone() else {
            return;
        };
        egui::CollapsingHeader::new(text!("Импорт и экспорт коллекций")).show(ui, |ui| {
            let path = root.display().escape_debug().to_string();
            ui.add(egui::Label::new(&path).truncate().selectable(true))
                .on_hover_text(&path);
            let source = match self.selection.as_slice() {
                [path] if collections::is_portable(path.path()) => Some(path.clone()),
                _ => None,
            };
            let import = ui.add_enabled(
                source.is_some(),
                egui::Button::new(text!("Импортировать выделенный файл")),
            );
            if import
                .on_disabled_hover_text(text!("Выберите файл .pawcollection.json в активной папке"))
                .clicked()
                && let Some(source) = source
            {
                let root = root.clone();
                self.start(ui.ctx(), move |progress| {
                    let group = collections::import(&root, &source, progress)?;
                    let mut group = group;
                    let states = group.refresh(progress)?;
                    Ok(ResultView::Imported(Update { group, states }))
                });
            }
            if let Some(index) = self.active {
                ui.horizontal_wrapped(|ui| {
                    let label = text!("Имя файла экспорта");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.export_name)
                            .hint_text(label)
                            .desired_width(245.0),
                    )
                    .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
                    if ui.button(text!("Экспортировать в эту папку")).clicked()
                    {
                        let mut group = catalog.collections[index].clone();
                        let name = self.export_name.clone();
                        self.start(ui.ctx(), move |progress| {
                            collections::export(&root, &name, &mut group, progress)
                                .map(ResultView::Exported)
                        });
                    }
                });
            }
        });
    }

    fn choose_group(&mut self, index: Option<usize>, catalog: &Catalog, ctx: &egui::Context) {
        self.active = index;
        self.selected = None;
        self.states.clear();
        self.filter.clear();
        self.notice = None;
        self.edit_name = None;
        self.remove_group = false;
        if let Some(group) = index.and_then(|index| catalog.collections.get(index))
            && !group.members.is_empty()
        {
            self.refresh(group, ctx);
        }
    }

    fn refresh(&mut self, group: &Collection, ctx: &egui::Context) {
        let group = group.clone();
        self.start(ctx, move |progress| update(group, None, progress));
    }

    fn start(
        &mut self,
        ctx: &egui::Context,
        work: impl FnOnce(&mut Progress) -> Result<ResultView, TransferError> + Send + 'static,
    ) {
        if !matches!(self.work, Phase::Running(_)) {
            self.notice = None;
            self.work = Phase::start(ctx, work);
        }
    }

    fn accept_result(&mut self, catalog: &mut Catalog) -> Option<Location> {
        if matches!(self.work, Phase::Running(_) | Phase::Ready) {
            return None;
        }
        match std::mem::replace(&mut self.work, Phase::Ready) {
            Phase::Finished(ResultView::Updated(update, notice)) => {
                if let Some(index) = self.active {
                    catalog.collections[index] = update.group;
                    self.states = update.states;
                    self.notice = notice.map(|message| (text(message).into(), false));
                }
            }
            Phase::Finished(ResultView::Imported(update)) => {
                match catalog.insert_imported(update.group) {
                    Ok(index) => {
                        self.active = Some(index);
                        self.selected = None;
                        self.states = update.states;
                        self.filter.clear();
                        self.edit_name = None;
                        self.remove_group = false;
                        self.notice = Some((text!("Коллекция импортирована").into(), false));
                    }
                    Err(error) => self.notice = Some((error, true)),
                }
            }
            Phase::Finished(ResultView::Exported(path)) => {
                let path = path.display().escape_debug().to_string();
                self.notice = Some((
                    crate::tr_format!("Collection exported: {path}", path = path),
                    false,
                ));
            }
            Phase::Finished(ResultView::Reveal(path)) => return Some(path),
            Phase::Failed(error) => self.notice = Some((error, true)),
            Phase::Stopped => self.notice = Some((text!("Операция остановлена").into(), false)),
            Phase::Running(_) | Phase::Ready => unreachable!(),
        }
        None
    }
}

fn update(
    mut group: Collection,
    notice: Option<crate::i18n::MessageKey>,
    progress: &mut Progress,
) -> Result<ResultView, TransferError> {
    let states = group.refresh(progress)?;
    Ok(ResultView::Updated(Update { group, states }, notice))
}

impl PawCommander {
    pub(super) fn open_collections(&mut self, return_focus: Option<egui::Id>, ctx: &egui::Context) {
        if self.comparison.is_some() || self.conflict_open() {
            return;
        }
        let root = self.tab().path.local().map(|_| self.tab().path.clone());
        let selection = if root.is_some() {
            self.selected()
        } else {
            Vec::new()
        };
        let mut dialog = CollectionsDialog::new(root, selection);
        dialog.choose_group(
            (!self.collections.collections.is_empty()).then_some(0),
            &self.collections,
            ctx,
        );
        self.comparison = Some(ComparisonDialog::new(
            ComparisonBody::Collections(Box::new(dialog)),
            return_focus,
            ctx,
        ));
    }
}
