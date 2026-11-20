use super::actions::SelectionChange;
use super::batch_rename::BatchRenameDialog;
use super::permissions::PermissionsDialog;
use super::selection::{Rule, RuleKind};
use super::state::Settings;
use super::{file_name, PawCommander};
use crate::archive::Format;
use crate::shortcuts::Command;
use crate::theme::Palette;
use crate::vfs::{ArchiveEdit, Endpoint, FileEntry, Location};
use crate::{icons, vfs as fs_ops};
use eframe::egui;
use eframe::egui::{Color32, RichText};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use zeroize::{Zeroize, Zeroizing};

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
    TransferLimits {
        draft: crate::operations::TransferLimits,
        endpoints: Vec<Endpoint>,
    },
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
    BatchRename(Box<BatchRenameDialog>),
    ChangePermissions(Box<PermissionsDialog>),
    #[cfg(target_os = "macos")]
    EditCustomActions(Box<super::custom_actions::Editor>),
    #[cfg(target_os = "macos")]
    RunCustomAction(Box<super::custom_actions::Preview>),
    #[cfg(target_os = "macos")]
    ExternalEditors(Box<super::external_editors::Editors>),
    #[cfg(target_os = "macos")]
    ConnectSmb(Box<super::smb_connections::Form>),
    CreateArchive {
        sources: Vec<Location>,
        directory: Location,
        name: String,
        format: Format,
        password: Zeroizing<String>,
    },
    ExtractArchive {
        sources: Vec<Location>,
        archive_directory: Location,
        other: Option<Location>,
        address: String,
        password: Zeroizing<String>,
    },
    VerifyArchive {
        archive: PathBuf,
        password: Zeroizing<String>,
    },
    CreateChecksumManifest {
        sources: Vec<Location>,
        directory: Location,
        name: String,
    },
    ArchiveEdit {
        archive: PathBuf,
        edit: ArchiveEdit,
        password: Zeroizing<String>,
    },
    Trash {
        paths: Vec<Location>,
    },
    WorkspaceName {
        editing: Option<usize>,
        name: String,
    },
}

fn archive_password_field(ui: &mut egui::Ui, password: &mut String, label: &'static str) {
    let label = ui.label(crate::i18n::text(label));
    let mut edit = egui::TextEdit::singleline(password)
        .id_salt("archive_password")
        .password(true)
        .desired_width(f32::INFINITY)
        .show(ui);
    edit.response.response.labelled_by(label.id);
    // The password field does not retain a second copy in egui's undo buffer.
    edit.state.clear_undoer();
}

fn archive_edit_command(edit: &ArchiveEdit) -> Command {
    match edit {
        ArchiveEdit::Add { .. } => Command::AddToArchive,
        ArchiveEdit::Remove(_) => Command::RemoveFromArchive,
        ArchiveEdit::Rename { .. } => Command::Rename,
    }
}

fn extraction_destination(
    address: &str,
    archive_directory: &Location,
    other: Option<&Location>,
) -> Result<Location, String> {
    if address.trim().is_empty() {
        return Err(crate::i18n::text("Укажите папку для распаковки").into());
    }
    if address == archive_directory.display() {
        return Ok(archive_directory.clone());
    }
    if let Some(other) = other
        && address == other.display()
    {
        return Ok(other.clone());
    }
    let base = if address.starts_with("sftp:") {
        other
            .filter(|location| location.remote())
            .unwrap_or(archive_directory)
    } else {
        archive_directory
    };
    let destination = fs_ops::resolve(address, base)?;
    if destination.is_archive() {
        Err(crate::archive::readonly().into())
    } else {
        Ok(destination)
    }
}

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
    pub(super) ssh_config: Option<crate::ssh_config::ResolvedHost>,
    pub(super) ssh_config_job: Option<Receiver<Result<crate::ssh_config::ResolvedHost, String>>>,
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
            ssh_config: None,
            ssh_config_job: None,
        }
    }
}

mod interactive_ssh;
mod ssh_import;
pub(super) use interactive_ssh::ConnectionJob;

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
        if let Some(job) = &mut self.connection {
            job.show(ctx);
            return;
        }
        let colors = Palette::from_ctx(ctx);
        let Some(form) = &mut self.sftp_form else {
            return;
        };
        form.poll_ssh_config();
        let busy = self.job.is_some();
        let loading_config = form.ssh_config_job.is_some();
        let mut submit = false;
        let mut cancel = false;
        let mut forget = false;
        let mut load_config = false;
        egui::Modal::new(egui::Id::unique("sftp_connect")).show(ctx, |ui| {
            ui.set_width(470.0);
            ui.heading(crate::i18n::text("Подключение SFTP"));
            ui.add_space(10.0);
            ui.add_enabled_ui(!busy && !loading_config, |ui| {
                egui::ComboBox::from_id_salt("saved_sftp_profiles").selected_text(crate::i18n::text("Сохранённые подключения")).show_ui(ui, |ui| {
                    for profile in &self.settings.sftp_profiles {
                        if ui.selectable_label(false, profile.display()).clicked()
                            && let Location::Sftp { endpoint, path } = profile {
                                form.host = endpoint.host.clone(); form.port = endpoint.port; form.user = endpoint.user.clone();
                                form.key = endpoint.key.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                                form.path = path.display().to_string(); form.password.clear(); form.passphrase.clear();
                                form.use_saved = true; form.save_key = false; form.error = None;
                                form.ssh_config = None;
                            }
                    }
                });
                egui::Grid::new("sftp_fields").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    let label = ui.label(crate::i18n::text("Сервер"));
                    ui.horizontal(|ui| {
                        let host = ui.add(egui::TextEdit::singleline(&mut form.host)
                            .desired_width(145.0)
                            .hint_text(crate::i18n::text("Имя или SSH-алиас"))).labelled_by(label.id);
                        if host.changed() {
                            form.clear_ssh_config();
                        }
                        load_config = ui.add_enabled(
                            !form.host.trim().is_empty(),
                            egui::Button::new(crate::i18n::text("Загрузить SSH-алиас")),
                        ).clicked();
                    });
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
                    interactive_ssh::answer_field(ui, &mut form.password, false).labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label(crate::i18n::text("Приватный ключ"));
                    ui.add(egui::TextEdit::singleline(&mut form.key).hint_text(crate::i18n::text("Путь к ключу (необязательно)"))).labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label(crate::i18n::text("Пароль ключа"));
                    interactive_ssh::answer_field(ui, &mut form.passphrase, false).labelled_by(label.id);
                    ui.end_row();
                });
                form.show_identity_files(ui);
                ui.add_space(10.0);
                ui.checkbox(&mut form.use_saved, crate::i18n::text("Использовать сохранённый ключ и фразу"));
                ui.checkbox(&mut form.save_key, crate::i18n::text("Сохранить ключ и фразу после подключения"));
                ui.weak(crate::i18n::text("Ключ и его фраза сохраняются в системном хранилище секретов. Пароль аккаунта не сохраняется. При сохранении читается выбранный файл ключа."));
                forget = ui.button(crate::i18n::text("Удалить сохранённый секрет")).clicked();
                ui.weak(crate::i18n::text("При первом подключении сравните отпечаток ключа SSH с данными администратора."));
            });
            form.show_config_result(ui);
            if let Some(error) = &form.error { ui.colored_label(colors.error, error); }
            ui.horizontal(|ui| {
                cancel = ui.add_enabled(!busy, egui::Button::new(crate::i18n::text("Отмена"))).clicked();
                let label = if form.ssh_config.as_ref().is_some_and(|config| config.matched && !config.unsupported.is_empty()) {
                    "Подключиться напрямую"
                } else {
                    "Подключиться"
                };
                submit = ui.add_enabled(!busy && !loading_config && !form.host.trim().is_empty() && !form.user.trim().is_empty(), egui::Button::new(crate::i18n::text(label))).clicked();
            });
            if busy { ui.horizontal(|ui| { ui.spinner(); ui.label(crate::i18n::text("Подключение…")); }); }
            if loading_config { ui.horizontal(|ui| { ui.spinner(); ui.label(crate::i18n::text("Читаем SSH config…")); }); }
        });
        if load_config {
            form.start_ssh_config(ctx, crate::ssh_config::load_user_config);
        } else if submit {
            let request = crate::vfs::ConnectRequest {
                endpoint: form.endpoint(),
                path: form.path.clone(),
                password: Zeroizing::new(std::mem::take(&mut form.password)),
                passphrase: Zeroizing::new(std::mem::take(&mut form.passphrase)),
                use_saved: form.use_saved,
                save_key: form.save_key,
            };
            match ConnectionJob::launch(self.active, request, ctx) {
                Ok(job) => {
                    form.error = None;
                    self.connection = Some(job);
                }
                Err(error) => form.error = Some(error),
            }
        } else if cancel {
            self.sftp_form = None;
        } else if forget {
            let endpoint = form.endpoint();
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
            Dialog::TransferLimits { .. } => crate::i18n::text("Скорость передачи"),
            Dialog::SelectionRule { command, .. } => command.label(),
            Dialog::NewFolder { .. } => crate::i18n::text("Новая папка"),
            Dialog::Rename { .. } => crate::i18n::text("Переименовать"),
            Dialog::BatchRename(_) => crate::i18n::text("Массовое переименование"),
            Dialog::ChangePermissions(_) => Command::ChangePermissions.label(),
            #[cfg(target_os = "macos")]
            Dialog::EditCustomActions(_) => Command::CustomActions.label(),
            #[cfg(target_os = "macos")]
            Dialog::RunCustomAction(_) => crate::i18n::text("Предпросмотр запуска"),
            #[cfg(target_os = "macos")]
            Dialog::ExternalEditors(_) => crate::i18n::text("Внешние редакторы"),
            #[cfg(target_os = "macos")]
            Dialog::ConnectSmb(_) => crate::i18n::text("Подключение SMB"),
            Dialog::CreateArchive { .. } => Command::CreateArchive.label(),
            Dialog::ExtractArchive { .. } => Command::ExtractArchive.label(),
            Dialog::VerifyArchive { .. } => Command::VerifyArchive.label(),
            Dialog::CreateChecksumManifest { .. } => Command::CreateChecksumManifest.label(),
            Dialog::ArchiveEdit { edit, .. } => archive_edit_command(edit).label(),
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
            let width: f32 = match &dialog {
                Dialog::BatchRename(_) | Dialog::ChangePermissions(_) => 605.0,
                #[cfg(target_os = "macos")]
                Dialog::EditCustomActions(_) | Dialog::RunCustomAction(_) | Dialog::ExternalEditors(_) => 605.0,
                _ => 430.0,
            };
            ui.set_width(width);
            ui.heading(title);
            ui.add_space(12.0);
            match &mut dialog {
                Dialog::TransferLimits { draft, endpoints } => {
                    super::transfer_limits::show_fields(ui, draft, endpoints);
                }
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
                Dialog::BatchRename(editor) => editor.show(ui, self.dialog_focus),
                Dialog::ChangePermissions(editor) => editor.show(ui, self.dialog_focus),
                #[cfg(target_os = "macos")]
                Dialog::EditCustomActions(editor) => editor.show(ui, self.dialog_focus),
                #[cfg(target_os = "macos")]
                Dialog::ExternalEditors(editors) => editors.show(ui),
                #[cfg(target_os = "macos")]
                Dialog::RunCustomAction(preview) => preview.show(ui),
                #[cfg(target_os = "macos")]
                Dialog::ConnectSmb(form) => form.show(ui, self.dialog_focus),
                Dialog::CreateChecksumManifest { sources, directory, name } => {
                    ui.label(crate::tr_format!("Выбрано файлов: {}", "Selected files: {}", sources.len()));
                    let label = ui.label(crate::i18n::text("Имя манифеста"));
                    let edit = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                    if self.dialog_focus {
                        edit.request_focus();
                    }
                    edit.labelled_by(label.id);
                    match fs_ops::checksum_manifest_name(name) {
                        Ok(file) => { ui.label(directory.join(file).display()); }
                        Err(error) => { ui.label(error); }
                    }
                }
                Dialog::CreateArchive { sources, directory, name, format, password } => {
                    ui.label(crate::tr_format!("Выбрано объектов: {}", "Selected items: {}", sources.len()));
                    let label = ui.label(crate::i18n::text("Имя архива"));
                    let edit = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                    edit.clone().labelled_by(label.id);
                    if self.dialog_focus {
                        edit.request_focus();
                    }
                    ui.horizontal(|ui| {
                        ui.label(crate::i18n::text("Формат архива"));
                        ui.selectable_value(format, Format::Zip, "ZIP");
                        ui.selectable_value(format, Format::Tar, "TAR");
                    });
                    if *format == Format::Zip {
                        archive_password_field(ui, password, "Пароль ZIP (необязательно)");
                    } else {
                        password.zeroize();
                    }
                    match fs_ops::archive_file_name(name, *format) {
                        Ok(file) => { ui.label(directory.join(file).display()); }
                        Err(error) => { ui.label(error); }
                    }
                }
                Dialog::ExtractArchive { sources, archive_directory, other, address, password } => {
                    ui.label(crate::tr_format!("Выбрано объектов: {}", "Selected items: {}", sources.len()));
                    let label = ui.label(crate::i18n::text("Папка для распаковки"));
                    let response = ui.add(egui::TextEdit::singleline(address).desired_width(f32::INFINITY)).labelled_by(label.id);
                    if self.dialog_focus {
                        response.request_focus();
                    }
                    if let Some(other) = other {
                        ui.horizontal_wrapped(|ui| {
                            if ui.button(crate::i18n::text("Папка с архивом")).clicked() {
                                *address = archive_directory.display();
                            }
                            if ui.button(crate::i18n::text("Папка в соседней панели")).clicked() {
                                *address = other.display();
                            }
                        });
                    }
                    if let Err(error) = extraction_destination(address, archive_directory, other.as_ref()) {
                        ui.colored_label(colors.error, error);
                    }
                    if sources.first().and_then(Location::archive_source).and_then(Format::from_path) == Some(Format::Zip) {
                        archive_password_field(ui, password, "Пароль ZIP (необязательно)");
                    }
                }
                Dialog::VerifyArchive { archive, password } => {
                    ui.label(archive.to_string_lossy());
                    if Format::from_path(archive) == Some(Format::Zip) {
                        archive_password_field(ui, password, "Пароль ZIP (необязательно)");
                    }
                }
                Dialog::ArchiveEdit { archive, edit, password } => {
                    ui.label(archive.to_string_lossy());
                    match edit {
                        ArchiveEdit::Add { folder, sources } => {
                            if !folder.as_os_str().is_empty() {
                                ui.label(crate::tr_format!(
                                    "Папка в архиве: {}", "Folder in archive: {}", folder.display()
                                ));
                            }
                            ui.label(crate::tr_format!(
                                "Выбрано объектов: {}", "Selected items: {}", sources.len()
                            ));
                            for source in sources.iter().take(6) {
                                ui.label(file_name(source));
                            }
                            if sources.len() > 6 {
                                ui.label(crate::tr_format!("…и ещё {}", "…and {} more", sources.len() - 6));
                            }
                            if Format::from_path(archive) == Some(Format::Zip) {
                                archive_password_field(ui, password, "Пароль добавляемых файлов ZIP");
                                ui.weak(crate::i18n::text("Для защищённого ZIP задайте пароль добавляемых файлов."));
                            }
                        }
                        ArchiveEdit::Rename { entry, name } => {
                            ui.label(entry.to_string_lossy());
                            let label = ui.label(crate::i18n::text("Новое имя"));
                            let response = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY)).labelled_by(label.id);
                            if self.dialog_focus {
                                response.request_focus();
                            }
                        }
                        ArchiveEdit::Remove(paths) => {
                            ui.label(crate::i18n::text("Удалить выбранные записи из архива?"));
                            for path in paths.iter().take(6) {
                                ui.label(path.to_string_lossy());
                            }
                            if paths.len() > 6 {
                                ui.label(crate::tr_format!("…и ещё {}", "…and {} more", paths.len() - 6));
                            }
                        }
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
                Dialog::BatchRename(editor) => editor.can_apply(),
                Dialog::ChangePermissions(editor) => editor.can_apply(),
                #[cfg(target_os = "macos")]
                Dialog::EditCustomActions(editor) => editor.can_save(),
                #[cfg(target_os = "macos")]
                Dialog::ExternalEditors(editors) => editors.valid(),
                #[cfg(target_os = "macos")]
                Dialog::RunCustomAction(preview) => preview.can_run() && self.application_job.is_none(),
                #[cfg(target_os = "macos")]
                Dialog::ConnectSmb(form) => form.endpoint().is_ok() && !self.smb.busy(),
                Dialog::CreateChecksumManifest { name, .. } => fs_ops::checksum_manifest_name(name).is_ok(),
                Dialog::CreateArchive { name, format, .. } => fs_ops::archive_file_name(name, *format).is_ok(),
                Dialog::ExtractArchive { address, archive_directory, other, .. } => extraction_destination(address, archive_directory, other.as_ref()).is_ok(),
                Dialog::ArchiveEdit { edit: ArchiveEdit::Rename { entry, name }, .. } => {
                    !name.trim().is_empty() && entry.file_name() != Some(std::ffi::OsStr::new(name.trim()))
                }
                Dialog::WorkspaceName { editing, name } => {
                    !name.trim().is_empty() && !self.workspace_name_taken(name, *editing)
                }
                Dialog::TransferLimits { .. } | Dialog::Trash { .. } | Dialog::VerifyArchive { .. } | Dialog::ArchiveEdit { .. } => true,
            };
            ui.horizontal(|ui| {
                let cancel_button = ui.button(crate::i18n::text("Отмена"));
                if self.dialog_focus && matches!(dialog, Dialog::Trash { .. }) {
                    cancel_button.request_focus();
                }
                #[cfg(target_os = "macos")]
                if self.dialog_focus && matches!(dialog, Dialog::RunCustomAction(_)) {
                    cancel_button.request_focus();
                }
                cancel = cancel_button.clicked();
                let label = if matches!(dialog, Dialog::Trash { .. }) {
                    if matches!(&dialog, Dialog::Trash { paths } if paths.iter().any(Location::remote)) { crate::i18n::text("Удалить навсегда") } else { crate::i18n::text("В корзину") }
                } else if matches!(dialog, Dialog::SelectionRule { .. }) {
                    crate::i18n::text("Выделить")
                } else if matches!(dialog, Dialog::BatchRename(_)) {
                    crate::i18n::text("Переименовать")
                } else if matches!(dialog, Dialog::ChangePermissions(_)) {
                    crate::i18n::text("Применить")
                } else if matches!(dialog, Dialog::CreateArchive { .. } | Dialog::CreateChecksumManifest { .. }) {
                    crate::i18n::text("Создать")
                } else if matches!(dialog, Dialog::ExtractArchive { .. }) {
                    crate::i18n::text("Распаковать")
                } else if matches!(dialog, Dialog::VerifyArchive { .. }) {
                    crate::i18n::text("Проверить")
                } else if let Dialog::ArchiveEdit { edit, .. } = &dialog {
                    crate::i18n::text(match edit {
                        ArchiveEdit::Add { .. } => "Добавить",
                        ArchiveEdit::Rename { .. } => "Переименовать",
                        ArchiveEdit::Remove(_) => "Удалить",
                    })
                } else {
                    crate::i18n::text(match &dialog {
                        #[cfg(target_os = "macos")]
                        Dialog::RunCustomAction(_) => "Запустить",
                        #[cfg(target_os = "macos")]
                        Dialog::ConnectSmb(_) => "Подключиться",
                        _ => "Сохранить",
                    })
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
                Dialog::TransferLimits { .. } | Dialog::Trash { .. } | Dialog::BatchRename(_) | Dialog::ChangePermissions(_) | Dialog::CreateArchive { .. } | Dialog::ExtractArchive { .. } | Dialog::VerifyArchive { .. } | Dialog::ArchiveEdit { .. } => false,
                #[cfg(target_os = "macos")]
                Dialog::EditCustomActions(_) | Dialog::RunCustomAction(_) | Dialog::ConnectSmb(_) | Dialog::ExternalEditors(_) => false,
                // The Enter that opens the dialog belongs to its sidebar control.
                Dialog::WorkspaceName { .. } | Dialog::SelectionRule { .. } | Dialog::CreateChecksumManifest { .. } => !self.dialog_focus,
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
        let guard_close_key = match dialog {
            Dialog::SelectionRule { .. } => true,
            #[cfg(target_os = "macos")]
            Dialog::EditCustomActions(_)
            | Dialog::RunCustomAction(_)
            | Dialog::ConnectSmb(_)
            | Dialog::ExternalEditors(_) => true,
            _ => false,
        };
        if guard_close_key && (confirm || cancel) {
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
                Dialog::TransferLimits { draft, .. } => self.apply_transfer_limits(draft, ctx),
                Dialog::SelectionRule { .. } => unreachable!("rule handled above"),
                #[cfg(target_os = "macos")]
                Dialog::EditCustomActions(editor) => {
                    self.settings.custom_actions = editor.into_templates();
                }
                #[cfg(target_os = "macos")]
                Dialog::ExternalEditors(editors) => self.settings.external_editors = *editors,
                #[cfg(target_os = "macos")]
                Dialog::RunCustomAction(preview) => {
                    if let Some(invocation) = preview.into_invocation() {
                        self.launch_custom_action(invocation, ctx);
                    }
                }
                #[cfg(target_os = "macos")]
                Dialog::ConnectSmb(form) => self.connect_smb(*form, ctx),
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
                Dialog::BatchRename(editor) => {
                    if let Some(plan) = (*editor).into_plan() {
                        self.start_batch_rename(ctx, plan);
                    }
                }
                Dialog::ChangePermissions(editor) => {
                    if let Some(plan) = (*editor).into_plan() {
                        let label = Command::ChangePermissions.label();
                        let description =
                            format!("{label} ({}) · {}", plan.changes, plan.directory());
                        self.start_stoppable_job(ctx, label, description, move |progress| {
                            plan.apply(progress)
                        });
                    }
                }
                Dialog::CreateArchive {
                    sources,
                    directory,
                    name,
                    format,
                    password,
                } => {
                    if let Ok(file) = fs_ops::archive_file_name(&name, format) {
                        let label = Command::CreateArchive.label();
                        let description = format!("{label} · {}", directory.join(file));
                        self.start_stoppable_job(ctx, label, description, move |progress| {
                            fs_ops::create_archive(
                                &sources,
                                &directory,
                                &name,
                                format,
                                Some(password.as_str()),
                                progress,
                            )
                        });
                    }
                }
                Dialog::ExtractArchive {
                    sources,
                    archive_directory,
                    other,
                    address,
                    password,
                } => {
                    if let Ok(directory) =
                        extraction_destination(&address, &archive_directory, other.as_ref())
                    {
                        let label = Command::ExtractArchive.label();
                        let description = format!("{label} · {}", directory.display());
                        self.start_conflict_job(
                            ctx,
                            label,
                            description,
                            move |progress, decide| {
                                fs_ops::extract_archive(
                                    &sources,
                                    &directory,
                                    Some(password.as_str()),
                                    progress,
                                    decide,
                                )
                            },
                        );
                    }
                }
                Dialog::VerifyArchive { archive, password } => {
                    let label = Command::VerifyArchive.label();
                    let description = format!("{label} · {}", archive.display());
                    self.start_stoppable_job(ctx, label, description, move |progress| {
                        crate::archive::verify_archive(&archive, Some(password.as_str()), progress)
                    });
                }
                Dialog::CreateChecksumManifest {
                    sources,
                    directory,
                    name,
                } => {
                    if let Ok(file) = fs_ops::checksum_manifest_name(&name) {
                        let label = Command::CreateChecksumManifest.label();
                        let description = format!("{label} · {}", directory.join(file));
                        self.start_stoppable_job(ctx, label, description, move |progress| {
                            fs_ops::create_checksum_manifest(&directory, &name, &sources, progress)
                        });
                    }
                }
                Dialog::ArchiveEdit {
                    archive,
                    edit,
                    password,
                } => {
                    let label = archive_edit_command(&edit).label();
                    let description = format!("{label} · {}", archive.display());
                    self.start_stoppable_job(ctx, label, description, move |progress| {
                        fs_ops::edit_archive(&archive, edit, Some(password.as_str()), progress)
                    });
                }
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
