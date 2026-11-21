//! Live name and content search in folders, plus indexed path search.
#[cfg(test)]
use crate::test_ui::ContextExt as _;
mod content;
mod directory;
mod filters;
mod index;
pub(crate) mod panel;
mod remote;
mod saved;

#[cfg(all(test, unix))]
pub(crate) use remote::check_sftp_search;

use crate::name_match::{normalize, Mask};
use crate::vfs::Location;
use content::{LineMatch, Scan, Scanner};
use directory::{Directory, Entry, EntryKind, EntryMetadata};
use eframe::egui;
use filters::{FilterInput, Filters};
use index::{IndexInfo, IndexStatus};
use std::{
    ffi::OsStr,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::{Rc, Weak},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
        Arc,
    },
    time::{Duration, Instant},
};

const MAX_RESULTS: usize = 1000;
// Keep directory handles and the traversal stack bounded for deeply nested trees.
const MAX_OPEN_DIRECTORIES: usize = 64;
const MAX_PATTERN_BYTES: usize = 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
const INDEX_INFO_BUDGET: Duration = Duration::from_millis(300);
const INDEX_REVEAL_WAIT: Duration = Duration::from_secs(2);

struct Results {
    matches: Vec<SearchMatch>,
    limited: bool,
    warning: String,
}

struct SearchMatch {
    path: Location,
    context: Option<LineMatch>,
}

impl SearchMatch {
    fn labels(&self) -> (String, String) {
        let address = self.path.display();
        if self.path.remote() {
            let single_line = |text: &str| {
                text.replace('\r', "\\r")
                    .replace('\n', "\\n")
                    .replace('\t', "\\t")
            };
            let name = single_line(&self.path.name());
            let label = match self.path.path().parent() {
                Some(parent) => format!("{name} · {}", single_line(&parent.to_string_lossy())),
                None => name,
            };
            return (label, address);
        }
        let label = if let Some(context) = &self.context {
            let line = context.line;
            crate::tr_format!("Строка {line} · {address}", "Line {line} · {address}")
        } else {
            address
        };
        (label.clone(), label)
    }
}

impl From<PathBuf> for SearchMatch {
    fn from(path: PathBuf) -> Self {
        Location::Local(path).into()
    }
}

impl From<Location> for SearchMatch {
    fn from(path: Location) -> Self {
        Self {
            path,
            context: None,
        }
    }
}

struct Job<T = Result<Results, String>> {
    cancel: Arc<AtomicBool>,
    result: Receiver<T>,
}

struct IndexReveal {
    // Keep a cancelled check in its slot until it returns from the filesystem.
    path: Option<PathBuf>,
    started: Instant,
    result: Receiver<(Instant, io::Result<()>)>,
}

struct IndexInspection {
    job: Job<IndexInfo>,
    status: Weak<IndexStatus>,
}

impl<T> Job<T> {
    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

impl<T> Drop for Job<T> {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Folder,
    Index,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum FolderTarget {
    #[default]
    Name,
    Text,
    Regex,
}

#[derive(Default)]
pub struct Search {
    pub visible: bool,
    pub(crate) panel: panel::Panel,
    focus: bool,
    mode: Mode,
    folder_target: FolderTarget,
    folder: Option<Location>,
    remote_seconds: Option<u64>,
    remote_deadline: Option<Instant>,
    query: String,
    exclusions: String,
    filters: FilterInput,
    saved: Vec<saved::SavedSearch>,
    saved_edit: Option<saved::Editor>,
    saved_popup: Option<egui::Id>,
    jobs: [Option<Job>; 2],
    results: Option<Results>,
    selected: Option<usize>,
    error: Option<String>,
    index_status: Option<Rc<IndexStatus>>,
    index_inspection: Option<IndexInspection>,
    index_reveal: Option<IndexReveal>,
}

impl Search {
    pub fn open(&mut self, folder: Option<&Location>) {
        let folder = folder.filter(|location| !location.is_archive());
        self.panel.cancel_pending();
        if self.folder.as_ref() != folder {
            if folder.is_some_and(Location::remote) {
                self.mode = Mode::Folder;
            }
            if self.mode == Mode::Folder {
                self.clear();
            }
            self.folder = folder.cloned();
        }
        self.visible = true;
        self.focus = true;
    }

    pub(crate) fn open_name_query(
        &mut self,
        folder: &Location,
        query: String,
        ctx: &egui::Context,
    ) {
        self.mode = Mode::Folder;
        self.open(Some(folder));
        self.folder_target = FolderTarget::Name;
        self.query = query;
        self.exclusions.clear();
        self.filters = FilterInput::default();
        self.start(None, ctx);
    }

    fn clear(&mut self) {
        self.cancel_work();
        self.results = None;
        self.selected = None;
        self.error = None;
        self.index_status = None;
    }

    fn cancel_work(&mut self) {
        for job in self.jobs.iter().flatten() {
            job.cancel();
        }
        if let Some(status) = &self.index_status {
            status.pending.set(None);
            if let Some(inspection) = &self.index_inspection
                && inspection.status.ptr_eq(&Rc::downgrade(status))
            {
                inspection.job.cancel();
            }
        }
        self.cancel_index_reveal();
    }

    fn job(&self) -> Option<&Job> {
        self.jobs[self.mode as usize].as_ref()
    }

    fn sftp_scope(&self) -> bool {
        self.mode == Mode::Folder && self.folder.as_ref().is_some_and(Location::remote)
    }

    fn target(&self) -> FolderTarget {
        if self.sftp_scope() {
            FolderTarget::Name
        } else {
            self.folder_target
        }
    }

    fn remote_time_limit(&self) -> u64 {
        self.remote_seconds
            .unwrap_or(remote::DEFAULT_SECONDS)
            .clamp(1, remote::MAX_SECONDS)
    }

    fn can_start(&self, cli: Option<&Path>) -> bool {
        let target = self.target();
        let has_query = if self.mode == Mode::Folder && target != FolderTarget::Name {
            !self.query.is_empty()
        } else {
            !self.query.trim().is_empty()
        };
        self.job().is_none()
            && match self.mode {
                Mode::Folder => {
                    self.folder.is_some()
                        && (has_query
                            || (target == FolderTarget::Name && self.filters.active(target)))
                }
                Mode::Index => cli.is_some() && has_query,
            }
    }

    fn start(&mut self, cli: Option<&Path>, ctx: &egui::Context) {
        if !self.can_start(cli) {
            return;
        }
        self.clear();
        let folder_target = self.target();
        let filters = if self.mode == Mode::Folder {
            match self.filters.parse(folder_target) {
                Ok(filters) => Some(filters),
                Err(error) => {
                    self.error = Some(error);
                    return;
                }
            }
        } else {
            None
        };
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let mode = self.mode;
        let folder = self.folder.clone();
        let remote_deadline = self
            .sftp_scope()
            .then(|| Instant::now() + Duration::from_secs(self.remote_time_limit()));
        let worker_cli = cli.map(Path::to_path_buf);
        let query = self.query.clone();
        let exclusions = self.exclusions.clone();
        let worker_ctx = ctx.clone();
        let worker = std::thread::Builder::new().spawn(move || {
            let result = match filters {
                Some(filters) => FolderSearch::new(folder_target, &query, &exclusions, filters)
                    .and_then(|search| {
                        let folder = folder.as_ref().expect("validated folder");
                        let cancelled = || worker_cancel.load(Ordering::Relaxed);
                        match remote_deadline {
                            Some(deadline) => remote::run(folder, &search, deadline, &cancelled),
                            None => run_folder(folder.path(), search, &cancelled),
                        }
                    }),
                None => run(
                    worker_cli.as_deref().expect("validated locate tool"),
                    &query,
                    &worker_cancel,
                    TIMEOUT,
                ),
            };
            let _ = tx.send(result);
            worker_ctx.request_repaint();
        });
        match worker {
            Ok(_) => {
                self.jobs[mode as usize] = Some(Job { cancel, result: rx });
                if mode == Mode::Folder {
                    self.remote_deadline = remote_deadline;
                }
                if mode == Mode::Index {
                    self.index_status = Some(Rc::new(IndexStatus::new(cli.map(Path::to_path_buf))));
                    self.poll_index_inspection(ctx);
                }
            }
            Err(error) => {
                self.error = Some(crate::tr_format!(
                    "Не удалось запустить поиск: {error}",
                    "Could not start search: {error}"
                ));
            }
        }
    }

    fn poll(&mut self) {
        for slot in &mut self.jobs {
            let Some(job) = slot else {
                continue;
            };
            let result = match job.result.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(crate::i18n::text!(
                    "Задача поиска завершилась неожиданно",
                )
                .into())),
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                if !job.cancelled() {
                    match result {
                        Ok(results) => self.results = Some(results),
                        Err(error) => self.error = Some(error),
                    }
                }
                *slot = None;
            }
        }
    }

    fn poll_index_inspection(&mut self, ctx: &egui::Context) {
        // A slow metadata call must not delay search results or start more workers.
        if let Some(inspection) = &self.index_inspection {
            let status = inspection.status.upgrade();
            if status.is_none() {
                inspection.job.cancel();
            }
            match inspection.job.result.try_recv() {
                Ok(info) => {
                    if let Some(status) = status
                        && !inspection.job.cancelled()
                    {
                        *status.info.borrow_mut() = info;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {}
            }
            self.index_inspection = None;
        }
        let request = self
            .panel
            .index_status()
            .into_iter()
            .chain(self.index_status.iter())
            .find_map(|status| {
                status
                    .pending
                    .take()
                    .map(|cli| (cli, Rc::downgrade(status)))
            });
        let Some((cli, status)) = request else {
            return;
        };
        let (tx, result) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let ctx = ctx.clone();
        if std::thread::Builder::new()
            .spawn(move || {
                let _ = tx.send(index::inspect(&cli, &worker_cancel, INDEX_INFO_BUDGET));
                ctx.request_repaint();
            })
            .is_ok()
        {
            self.index_inspection = Some(IndexInspection {
                job: Job { cancel, result },
                status,
            });
        }
    }

    fn cancel_index_reveal(&mut self) {
        if let Some(check) = &mut self.index_reveal {
            check.path = None;
        }
    }

    fn check_index_reveal(&mut self, path: PathBuf, ctx: &egui::Context) {
        if self.index_reveal.is_some() {
            self.error = Some(
                crate::i18n::text!("Предыдущая проверка пути ещё ждёт ответа файловой системы.")
                    .into(),
            );
            ctx.request_repaint();
            return;
        }
        let (tx, result) = mpsc::channel();
        let candidate = path.clone();
        let ctx = ctx.clone();
        let started = Instant::now();
        let worker = std::thread::Builder::new().spawn(move || {
            // A dangling symlink is itself a file-manager entry.
            let result = candidate.symlink_metadata().map(|_| ());
            let _ = tx.send((Instant::now(), result));
            ctx.request_repaint();
        });
        match worker {
            Ok(_) => {
                self.error = None;
                self.index_reveal = Some(IndexReveal {
                    path: Some(path),
                    started,
                    result,
                });
            }
            Err(error) => {
                self.error = Some(crate::tr_format!(
                    "Не удалось проверить путь из индекса. {error}",
                    "Could not check the indexed path. {error}"
                ));
            }
        }
    }

    fn poll_index_reveal(&mut self, ctx: &egui::Context) -> Option<PathBuf> {
        let check = self.index_reveal.as_mut()?;
        let result = match check.result.try_recv() {
            Ok((finished, _)) if finished.duration_since(check.started) >= INDEX_REVEAL_WAIT => {
                Err(io::ErrorKind::TimedOut.into())
            }
            Ok((_, result)) => result,
            Err(mpsc::TryRecvError::Empty) => {
                if check.started.elapsed() >= INDEX_REVEAL_WAIT && check.path.take().is_some() {
                    self.error = Some(
                        crate::i18n::text!(
                            "Файловая система не ответила на проверку пути из индекса.",
                        )
                        .into(),
                    );
                    ctx.request_repaint();
                }
                return None;
            }
            Err(mpsc::TryRecvError::Disconnected) => Err(io::ErrorKind::Interrupted.into()),
        };
        ctx.request_repaint();
        let path = self.index_reveal.take()?.path?;
        match result {
            Ok(()) => return Some(path),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                if let Some(results) = &mut self.results {
                    results
                        .matches
                        .retain(|hit| hit.path.local() != Some(&path));
                }
                self.selected = None;
                let path = path.display();
                self.error = Some(crate::tr_format!(
                    "Путь из индекса больше не существует: {path}",
                    "The indexed path no longer exists: {path}"
                ));
            }
            Err(error) => {
                self.error = Some(if error.kind() == io::ErrorKind::TimedOut {
                    crate::i18n::text!("Файловая система не ответила на проверку пути из индекса.")
                        .into()
                } else {
                    crate::tr_format!(
                        "Не удалось проверить путь из индекса. {error}",
                        "Could not check the indexed path. {error}"
                    )
                });
            }
        }
        None
    }

    fn checking_index_reveal(&self) -> bool {
        self.index_reveal
            .as_ref()
            .is_some_and(|check| check.path.is_some())
    }

    pub fn show(&mut self, ctx: &egui::Context, cli: Option<&Path>) -> Option<Location> {
        self.poll();
        self.poll_index_inspection(ctx);
        if !self.visible {
            self.close_saved_popup(ctx);
            self.poll_index_reveal(ctx);
            return None;
        }
        let mut visible = true;
        let mut reveal = None;
        let mut dock_results = false;
        // egui closes an open popup while drawing it, leaving Escape unconsumed.
        let popup_was_open = egui::Popup::is_any_open(ctx);
        let window = egui::Window::new(crate::i18n::text!("Поиск файлов"))
            .id(egui::Id::unique("file_search"))
            .open(&mut visible)
            .default_width(720.0)
            .vscroll(true)
            .drag_to_scroll(egui::scroll_area::DragScroll::Never)
            .show(ctx, |ui| {
                let previous_mode = self.mode;
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.mode, Mode::Folder, crate::i18n::text!("В папке и подпапках"));
                    ui.selectable_value(&mut self.mode, Mode::Index, crate::i18n::text!("Индекс locate"));
                });
                if self.mode != previous_mode {
                    self.clear();
                    self.focus = true;
                }
                let mut submit = false;
                let hint = match self.mode {
                    Mode::Folder => {
                        if let Some(folder) = &self.folder {
                            let path = folder.display();
                            let label = crate::tr_format!("Папка поиска: {path}", "Search folder: {path}");
                            ui.add(egui::Label::new(&label).truncate()).on_hover_text(label);
                            let exclusions = ui.add(
                                egui::TextEdit::singleline(&mut self.exclusions)
                                    .hint_text(crate::i18n::text!("Исключить имена через запятую, например .git, target, *.tmp"))
                                    .desired_width(ui.available_width()),
                            );
                            ctx.accesskit_node_builder(exclusions.id, |node| {
                                node.set_label(crate::i18n::text!("Исключения поиска"));
                            });
                            if exclusions.changed() {
                                self.clear();
                            }
                            submit = exclusions.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        } else {
                            ui.label(crate::i18n::text!("Откройте локальную папку или папку SFTP для поиска."));
                        }
                        if self.sftp_scope() {
                            let mut seconds = self.remote_time_limit();
                            ui.horizontal(|ui| {
                                let label = ui.label(crate::i18n::text!("Время поиска по SFTP, с"));
                                ui.add(egui::DragValue::new(&mut seconds).range(1..=remote::MAX_SECONDS)).labelled_by(label.id);
                                let depth = remote::DEFAULT_DEPTH;
                                ui.small(crate::tr_format!("Глубина по умолчанию — {depth}. Меняется в фильтрах.", "Default depth is {depth}. Change it in Filters."));
                            });
                            if seconds != self.remote_time_limit() {
                                self.remote_seconds = Some(seconds);
                                self.clear();
                            }
                        } else {
                            let previous_target = self.folder_target;
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut self.folder_target, FolderTarget::Name, crate::i18n::text!("Имя файла"));
                                ui.selectable_value(&mut self.folder_target, FolderTarget::Text, crate::i18n::text!("Текст файла"));
                                ui.selectable_value(&mut self.folder_target, FolderTarget::Regex, crate::i18n::text!("Регулярное выражение"));
                            });
                            if self.folder_target != previous_target {
                                self.clear();
                                self.focus = true;
                            }
                        }
                        let target = self.target();
                        if target != FolderTarget::Name {
                            ui.small(crate::i18n::text!("Поиск по строкам в файлах UTF-8 до 1 МиБ, с учётом регистра по умолчанию."));
                        }
                        if target == FolderTarget::Regex {
                            ui.small(crate::i18n::text!("При поиске по выражению строки больше 64 КиБ пропускаются."));
                        }
                        match target {
                            FolderTarget::Name if self.filters.active(target) => {
                                crate::i18n::text!("Имя необязательно при заданных фильтрах")
                            }
                            FolderTarget::Name => crate::i18n::text!("Часть имени или маска: отчёт, *.rs, фото?.jpg"),
                            FolderTarget::Text => crate::i18n::text!("Текст для поиска, например TODO"),
                            FolderTarget::Regex => crate::i18n::text!("Регулярное выражение, например TODO\\s*:"),
                        }
                    }
                    Mode::Index => {
                        ui.label(crate::i18n::text!("Поиск по полному пути в локальном индексе, без учёта регистра."));
                        ui.label(crate::i18n::text!("База может отставать от файловой системы; SFTP не индексируется."));
                        if let Some(cli) = cli {
                            ui.small(format!("locate: {}", cli.display()));
                        } else {
                            ui.colored_label(ui.visuals().error_fg_color, crate::i18n::text!("locate не найден"));
                            ui.label(crate::i18n::text!("На Linux установите locate/plocate; на macOS используется /usr/bin/locate. Затем: Настройки → Инструменты → Проверить снова."));
                        }
                        crate::i18n::text!("Часть пути или маска: Cargo, *.rs, */Documents/*")
                    }
                };
                ui.horizontal(|ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text(hint)
                            .desired_width((ui.available_width() - 175.0).max(120.0)),
                    );
                    ctx.accesskit_node_builder(response.id, |node| {
                        node.set_label(crate::i18n::text!("Запрос поиска"));
                    });
                    if self.focus {
                        response.request_focus();
                        self.focus = false;
                    }
                    if response.changed() {
                        self.clear();
                    }
                    submit |= response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    submit |= ui.add_enabled(
                        self.can_start(cli),
                        egui::Button::new(crate::i18n::text!("Найти")),
                    ).clicked();
                    if let Some(job) = self.job() {
                        ui.spinner();
                        if job.cancelled() {
                            ui.label(crate::i18n::text!("Остановка поиска…"));
                        } else if ui.button(crate::i18n::text!("Отмена")).clicked() {
                            self.cancel_work();
                            self.error = Some(crate::i18n::text!("Поиск отменён").into());
                        }
                    }
                });
                if self.mode == Mode::Folder && self.job().is_some_and(|job| !job.cancelled())
                    && self.remote_deadline.is_some_and(|deadline| Instant::now() >= deadline)
                {
                    ui.small(crate::i18n::text!("Лимит времени достигнут. Завершается текущий запрос SFTP…"));
                }
                if self.mode == Mode::Folder && self.folder.is_some() {
                    let target = self.target();
                    let (changed, filter_submit) = self.filters.show(ui, target);
                    if changed {
                        self.clear();
                    }
                    submit |= filter_submit;
                }
                if submit {
                    self.start(cli, ctx);
                }
                self.show_saved(ui, cli);
                if let Some(error) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                if self.mode == Mode::Index {
                    if let Some(status) = &self.index_status {
                        let info = status.info.borrow();
                        ui.small(&info.source);
                        ui.small(&info.freshness);
                    }
                    ui.small(crate::i18n::text!("Если база отсутствует или устарела, обновите её средствами ОС (locate.updatedb / updatedb)."));
                }
                if let Some(results) = &self.results {
                    ui.separator();
                    let has_context = self.mode == Mode::Folder && self.target() != FolderTarget::Name;
                    let count = results.matches.len();
                    let limit = if results.limited { crate::i18n::text!("+ · Уточните запрос, показаны первые 1000") } else { "" };
                    ui.label(if has_context {
                        crate::tr_format!("Строк с совпадениями: {count}{limit}", "Matching lines: {count}{limit}")
                    } else {
                        crate::tr_format!("Найдено: {count}{limit}", "Found: {count}{limit}")
                    });
                    if !results.warning.is_empty() {
                        ui.colored_label(ui.visuals().warn_fg_color, &results.warning);
                    }
                    if results.matches.is_empty() && self.error.is_none() {
                        ui.label(match self.mode {
                            Mode::Folder if has_context => crate::i18n::text!("В просмотренных файлах совпадений нет."),
                            Mode::Folder => crate::i18n::text!("В просмотренных папках совпадений нет."),
                            Mode::Index => crate::i18n::text!("Совпадений в индексе нет. Проверьте запрос и актуальность базы locate."),
                        });
                    }
                    let excerpt_height = ui.text_style_height(&egui::TextStyle::Body);
                    let button_height = 28.0_f32.max(excerpt_height);
                    let can_reveal = !self.checking_index_reveal();
                    let dock = ui.add_enabled(
                        can_reveal,
                        egui::Button::new(crate::i18n::text!("Показать результаты отдельной панелью")),
                    );
                    if dock.clicked() {
                        dock.surrender_focus();
                        dock_results = true;
                    }
                    let row_height = if has_context {
                        button_height + excerpt_height + ui.spacing().item_spacing.y
                    } else {
                        button_height
                    };
                    egui::ScrollArea::vertical()
                        .id_salt("search_results")
                        .max_height((ctx.content_rect().height() * 0.45).min(340.0))
                        .show_rows(ui, row_height, results.matches.len(), |ui, rows| {
                            for index in rows {
                                let hit = &results.matches[index];
                                let line = hit.context.as_ref().map(|context| context.line);
                                let (label, tooltip) = hit.labels();
                                ui.push_id((&hit.path, line), |ui| {
                                    let response = ui.add(
                                        egui::Button::new(&label)
                                            .small()
                                            .selected(self.selected == Some(index))
                                            .truncate()
                                            .min_size(egui::vec2(ui.available_width(), button_height)),
                                    ).on_hover_text(tooltip);
                                    if response.clicked() {
                                        self.selected = Some(index);
                                    }
                                    if response.double_clicked() && can_reveal {
                                        reveal = Some(hit.path.clone());
                                    }
                                    if let Some(context) = &hit.context {
                                        ui.add(
                                            egui::Label::new(&context.excerpt)
                                                .truncate()
                                                .selectable(false),
                                        ).on_hover_text(&context.excerpt);
                                    }
                                });
                            }
                        });
                    if ui.add_enabled(self.selected.is_some() && can_reveal, egui::Button::new(crate::i18n::text!("Показать в панели"))).clicked() {
                        reveal = self.selected.map(|index| results.matches[index].path.clone());
                    }
                    if !can_reveal {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(crate::i18n::text!("Проверка пути из индекса…"));
                        });
                    }
                    ui.small(crate::i18n::text!("Двойной щелчок — перейти к результату в активной панели."));
                }
            });
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            if popup_was_open {
                egui::Popup::close_all(ctx);
            } else if self.saved_edit.take().is_none() {
                visible = false;
            }
        }
        if visible
            && dock_results
            && let Some(results) = self.results.take()
        {
            let matching_lines = self.mode == Mode::Folder && self.target() != FolderTarget::Name;
            self.panel.replace(
                results,
                matching_lines,
                self.query.clone(),
                self.index_status.take(),
            );
            self.selected = None;
            visible = false;
            ctx.request_repaint();
        }
        if !visible {
            self.cancel_index_reveal();
            reveal = None;
        }
        let checked = self.poll_index_reveal(ctx);
        if self.mode == Mode::Index {
            if let Some(Location::Local(path)) = reveal.take() {
                self.check_index_reveal(path, ctx);
            }
            reveal = checked.map(Location::Local);
        }
        self.visible = visible && reveal.is_none();
        if !self.visible || window.and_then(|response| response.inner).is_none() {
            self.close_saved_popup(ctx);
        }
        if !self.visible {
            self.saved_edit = None;
            self.cancel_work();
        } else if self.job().is_some() || self.checking_index_reveal() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        reveal
    }
}

enum FolderMatcher<'a> {
    Name { query: String, mask: Option<Mask> },
    Content(Scanner<'a>),
}

struct FolderSearch<'a> {
    matcher: FolderMatcher<'a>,
    exclusions: Vec<Mask>,
    filters: Filters,
}

fn validate_folder_query(
    target: FolderTarget,
    query: &str,
    exclusions: &str,
    has_filters: bool,
) -> Result<(), String> {
    let missing_query = match target {
        FolderTarget::Name => query.trim().is_empty() && !has_filters,
        FolderTarget::Text | FolderTarget::Regex => query.is_empty(),
    };
    if missing_query || query.contains('\0') {
        return Err(crate::i18n::text!("Введите непустой запрос без NUL").into());
    }
    if query.len() > MAX_PATTERN_BYTES
        || exclusions.len() > MAX_PATTERN_BYTES
        || exclusions.contains('\0')
    {
        return Err(crate::i18n::text!("Укоротите запрос или список исключений.").into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn validate_name_query(query: &str) -> Result<(), String> {
    validate_folder_query(FolderTarget::Name, query, "", false)
}

impl<'a> FolderSearch<'a> {
    fn new(
        target: FolderTarget,
        query: &'a str,
        exclusions: &str,
        filters: Filters,
    ) -> Result<Self, String> {
        validate_folder_query(target, query, exclusions, filters.active())?;
        let matcher = match target {
            FolderTarget::Name => {
                let query = if query.trim().is_empty() { "" } else { query };
                let query = normalize(query).into_owned();
                let mask = query.contains(['*', '?']).then(|| Mask::new(&query));
                FolderMatcher::Name { query, mask }
            }
            FolderTarget::Text | FolderTarget::Regex => {
                FolderMatcher::Content(Scanner::new(query, target == FolderTarget::Regex)?)
            }
        };
        let exclusions = exclusions
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| Mask::new(&normalize(name)))
            .collect();
        Ok(Self {
            matcher,
            exclusions,
            filters,
        })
    }

    fn excluded(&self, name: &OsStr) -> bool {
        if self.exclusions.is_empty() {
            return false;
        }
        let name = name.to_string_lossy();
        let normalized = normalize(&name);
        self.exclusions.iter().any(|mask| mask.matches(&normalized))
    }

    fn matches_name(&self, name: &OsStr, kind: Option<EntryKind>) -> bool {
        if !self.filters.matches_kind(kind) || !self.filters.matches_extension(name) {
            return false;
        }
        if let FolderMatcher::Name { query, mask } = &self.matcher
            && !query.is_empty()
        {
            let name = name.to_string_lossy();
            let normalized = normalize(&name);
            let found = match mask {
                Some(mask) => mask.matches(&normalized),
                None => normalized.contains(query.as_str()),
            };
            if !found {
                return false;
            }
        }
        true
    }

    fn matches_candidate(&self, directory: &Directory, entry: &Entry) -> io::Result<bool> {
        if !self.matches_name(&entry.name, entry.kind.as_ref().ok().copied()) {
            return Ok(false);
        }
        if self.filters.needs_metadata() {
            return self
                .filters
                .matches_metadata(directory.entry_metadata(&entry.name)?);
        }
        Ok(true)
    }
}

fn check_cancelled(cancelled: &impl Fn() -> bool) -> Result<(), String> {
    if cancelled() {
        Err(crate::i18n::text!("Поиск отменён").into())
    } else {
        Ok(())
    }
}

fn run_folder(
    folder: &Path,
    mut search: FolderSearch<'_>,
    cancelled: &impl Fn() -> bool,
) -> Result<Results, String> {
    check_cancelled(cancelled)?;

    let directory = Directory::open(folder.to_path_buf()).map_err(|error| {
        crate::tr_format!(
            "Не удалось прочитать папку: {error}",
            "Could not read folder: {error}"
        )
    })?;
    let mut directories = vec![directory];
    let mut matches = Vec::new();
    let mut depth_limited = false;
    let mut unreadable = 0;
    let mut too_large = 0;
    let mut binary = 0;
    let mut long_lines = 0;
    loop {
        check_cancelled(cancelled)?;
        let at_depth_limit =
            directories.len() == search.filters.depth.unwrap_or(MAX_OPEN_DIRECTORIES);
        let Some(directory) = directories.last_mut() else {
            break;
        };
        let entry = match directory.next_entry() {
            Ok(Some(entry)) => entry,
            Err(_) => {
                unreadable += 1;
                directories.pop();
                continue;
            }
            Ok(None) => {
                directories.pop();
                continue;
            }
        };
        if search.excluded(&entry.name) {
            continue;
        }
        let candidate = match search.matches_candidate(directory, &entry) {
            Ok(candidate) => candidate,
            Err(_) => {
                unreadable += 1;
                false
            }
        };
        match &mut search.matcher {
            FolderMatcher::Name { .. } if candidate => {
                matches.push(directory.path.join(&entry.name).into());
            }
            FolderMatcher::Content(scanner) if candidate => {
                match directory.open_file(&entry.name) {
                    Ok(Some((file, metadata))) => {
                        if search.filters.needs_metadata() {
                            // The file may have changed since the preliminary no-follow stat.
                            match search
                                .filters
                                .matches_metadata(EntryMetadata::for_open_file(&metadata))
                            {
                                Ok(true) => {}
                                Ok(false) => continue,
                                Err(_) => {
                                    unreadable += 1;
                                    continue;
                                }
                            }
                        }
                        let remaining = MAX_RESULTS + 1 - matches.len();
                        match scanner.scan(file, metadata, remaining, cancelled)? {
                            Scan::Text {
                                matches: lines,
                                long_lines: skipped,
                            } => {
                                long_lines += skipped;
                                if !lines.is_empty() {
                                    let path = Location::Local(directory.path.join(&entry.name));
                                    matches.extend(lines.into_iter().map(|line| SearchMatch {
                                        path: path.clone(),
                                        context: Some(line),
                                    }));
                                }
                            }
                            Scan::TooLarge => too_large += 1,
                            Scan::Binary => binary += 1,
                            Scan::Unreadable => unreadable += 1,
                        }
                    }
                    Ok(None) => {}
                    Err(_) => unreadable += 1,
                }
            }
            _ => {}
        }
        if matches.len() > MAX_RESULTS {
            break;
        }
        match entry.kind {
            Ok(EntryKind::File | EntryKind::Other) => {}
            Ok(EntryKind::Directory) if at_depth_limit => {
                depth_limited |= search.filters.depth.is_none();
            }
            Ok(EntryKind::Directory) => match directory.open_child(&entry.name) {
                Ok(Some(directory)) => directories.push(directory),
                Ok(None) => {}
                Err(_) => unreadable += 1,
            },
            Err(_) => unreadable += 1,
        }
    }
    let limited = matches.len() > MAX_RESULTS;
    matches.truncate(MAX_RESULTS);
    matches.sort_unstable_by(|a, b| {
        a.path.cmp(&b.path).then_with(|| {
            a.context
                .as_ref()
                .map(|context| context.line)
                .cmp(&b.context.as_ref().map(|context| context.line))
        })
    });
    let mut warnings = Vec::new();
    if unreadable > 0 {
        warnings.push(crate::tr_format!(
            "Не удалось прочитать некоторые файлы или папки ({unreadable}).",
            "Some files or folders could not be read ({unreadable})."
        ));
    }
    if depth_limited {
        warnings
            .push(crate::i18n::text!("Поиск пропустил слишком глубоко вложенные папки.").into());
    }
    if too_large > 0 {
        warnings.push(crate::tr_format!(
            "Пропущены файлы больше 1 МиБ ({too_large}).",
            "Skipped files larger than 1 MiB ({too_large})."
        ));
    }
    if binary > 0 {
        warnings.push(crate::tr_format!(
            "Пропущены двоичные файлы и файлы не в UTF-8 ({binary}).",
            "Skipped binary or non-UTF-8 files ({binary})."
        ));
    }
    if long_lines > 0 {
        warnings.push(crate::tr_format!(
            "Пропущены строки больше 64 КиБ ({long_lines}).",
            "Skipped lines longer than 64 KiB ({long_lines})."
        ));
    }
    Ok(Results {
        matches,
        limited,
        warning: warnings.join("\n"),
    })
}

fn command(cli: &Path, query: &str) -> Command {
    let mut command = Command::new(cli);
    command
        .args(["-0", "-i", "-l", &(MAX_RESULTS + 1).to_string(), "--"])
        .arg(query)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn read_capped(reader: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::other(crate::i18n::text!(
            "Превышен лимит вывода locate; уточните запрос",
        )));
    }
    Ok(bytes)
}

fn validate_index_query(query: &str) -> Result<(), String> {
    if query.trim().is_empty() || query.contains('\0') {
        return Err(crate::i18n::text!("Введите непустой запрос без NUL").into());
    }
    Ok(())
}

fn run(cli: &Path, query: &str, cancel: &AtomicBool, timeout: Duration) -> Result<Results, String> {
    validate_index_query(query)?;
    let mut child = command(cli, query).spawn().map_err(|e| {
        crate::tr_format!(
            "Не удалось запустить locate: {e}",
            "Could not start locate: {e}"
        )
    })?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // Drain both pipes concurrently; neither memory nor process time is unbounded.
    let out = std::thread::spawn(move || read_capped(stdout, 8 * 1024 * 1024));
    let err = std::thread::spawn(move || read_capped(stderr, 16 * 1024));
    let started = Instant::now();
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            break Err(crate::i18n::text!("Поиск отменён").to_string());
        }
        if started.elapsed() >= timeout {
            break Err(crate::i18n::text!(
                "locate не ответил за 15 секунд; проверьте базу индекса"
            )
            .to_string());
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                break Err(crate::tr_format!(
                    "Ошибка ожидания locate: {e}",
                    "Error waiting for locate: {e}"
                ))
            }
        }
    };
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let stdout = out
        .join()
        .map_err(|_| crate::i18n::text!("Ошибка чтения вывода locate"))?;
    let stderr = err
        .join()
        .map_err(|_| crate::i18n::text!("Ошибка чтения ошибок locate"))?;
    let status = status?;
    let stdout = stdout.map_err(|e| e.to_string())?;
    let stderr = stderr.map_err(|e| e.to_string())?;
    let warning = String::from_utf8_lossy(&stderr).trim().to_string();
    // GNU locate/plocate return 1 for no matches. BSD may return 0 instead.
    let no_matches = status.code() == Some(1) && stdout.is_empty() && warning.is_empty();
    if !(status.success() || no_matches) {
        return Err(crate::tr_format!("Ошибка locate ({status}). {warning}\nПроверьте наличие и актуальность базы locate.", "locate error ({status}). {warning}\nCheck that the locate index exists and is up to date."
        ));
    }
    let mut paths = Vec::new();
    for bytes in stdout.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let path = path_from_bytes(bytes)?;
        if path.is_absolute() {
            paths.push(path);
        }
    }
    let limited = paths.len() > MAX_RESULTS;
    paths.truncate(MAX_RESULTS);
    paths.sort();
    paths.dedup();
    Ok(Results {
        matches: paths.into_iter().map(SearchMatch::from).collect(),
        limited,
        warning,
    })
}

fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(std::ffi::OsString::from_vec(bytes.to_vec()).into())
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes.to_vec())
            .map(PathBuf::from)
            .map_err(|_| crate::i18n::text!("locate вернул путь не в UTF-8").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, fs};

    #[test]
    fn sftp_search_result_shows_readable_filename_and_preserves_its_address() {
        let path = crate::vfs::resolve(
            "sftp://user@example.test/team/yyyy-mm/Отчёт%20A%0AB.txt",
            &Location::Local(PathBuf::from("/")),
        )
        .unwrap();
        let address = path.display();
        let (label, tooltip) = SearchMatch::from(path).labels();
        assert_eq!(label, "Отчёт A\\nB.txt · /team/yyyy-mm");
        assert_eq!(tooltip, address);
        assert!(tooltip.contains("%0A"));
    }

    fn run_folder(
        folder: &Path,
        target: FolderTarget,
        query: &str,
        exclusions: &str,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Results, String> {
        let search = FolderSearch::new(
            target,
            query,
            exclusions,
            FilterInput::default().parse(target)?,
        )?;
        super::run_folder(folder, search, cancelled)
    }

    fn paths(results: &Results) -> Vec<PathBuf> {
        results
            .matches
            .iter()
            .map(|hit| hit.path.path().to_path_buf())
            .collect()
    }

    #[test]
    fn escape_dismisses_the_filter_dropdown_before_closing_search() {
        use egui::accesskit::{Action, ActionRequest};

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        ctx.global_style_mut(|style| style.animation_time = 0.0);
        let root = tempfile::tempdir().unwrap();
        let mut search = Search::default();
        search.open(Some(&Location::Local(root.path().to_path_buf())));

        let mut draw = |events| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 400.0),
                )),
                events,
                ..Default::default()
            };
            ctx.run_test_ui(input, |root_ui| {
                let ctx = &root_ui.ctx().clone();
                search.show(ctx, None);
            })
        };
        let click = |output: egui::FullOutput, label| {
            let nodes = output.platform_output.accesskit_update.unwrap().nodes;
            let (target, _) = nodes
                .into_iter()
                .find(|(_, node)| {
                    node.label() == Some(label) && node.supports_action(Action::Click)
                })
                .expect("accessible search control");
            vec![egui::Event::AccessKitActionRequest(ActionRequest {
                action: Action::Click,
                target_node: target,
                target_tree: egui::accesskit::TreeId::ROOT,
                data: None,
            })]
        };
        let _ = draw(vec![]);
        let open_filters = click(draw(vec![]), crate::i18n::text!("Фильтры"));
        let filters = draw(open_filters);
        let open_type_menu = click(filters, crate::i18n::text!("Тип объекта"));
        let _ = draw(open_type_menu);
        assert!(egui::Popup::is_any_open(&ctx));

        let escape = |pressed| egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let after_popup = draw(vec![escape(true), escape(false)]);
        assert!(!egui::Popup::is_any_open(&ctx));
        let _ = click(after_popup, crate::i18n::text!("Тип объекта"));
        // Changing the search target can remove a combo box before it processes Escape.
        egui::Popup::open_id(&ctx, egui::Id::unique("removed_filter_popup"));
        let after_removed_popup = draw(vec![escape(true), escape(false)]);
        assert!(!egui::Popup::is_any_open(&ctx));
        let _ = click(after_removed_popup, crate::i18n::text!("Тип объекта"));
        let _ = draw(vec![escape(true)]);
        assert!(!search.visible);
    }

    #[test]
    fn closing_search_cancels_work_and_missing_tool_does_not_start_it() {
        let ctx = egui::Context::default();
        let mut search = Search {
            query: "Cargo".into(),
            mode: Mode::Index,
            ..Default::default()
        };
        search.open(None);
        let _ = ctx.run_test_ui(egui::RawInput::default(), |root_ui| {
            let ctx = &root_ui.ctx().clone();
            search.show(ctx, None);
        });
        assert!(search.visible);
        assert!(!search.can_start(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        search.jobs[Mode::Index as usize] = Some(Job {
            cancel: cancel.clone(),
            result: rx,
        });
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let _ = ctx.run_test_ui(input, |root_ui| {
            let ctx = &root_ui.ctx().clone();
            search.show(ctx, None);
        });
        assert!(!search.visible);
        assert!(cancel.load(Ordering::Relaxed));
        tx.send(Ok(Results {
            matches: vec![PathBuf::from("/stale").into()],
            limited: false,
            warning: String::new(),
        }))
        .unwrap();
        search.poll();
        assert!(search.job().is_none());
        assert!(search.results.is_none());
    }

    #[test]
    fn folder_search_starts_without_locate_or_a_retiring_index_worker() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("Fresh.txt");
        fs::write(&path, "").unwrap();
        let (_tx, rx) = mpsc::channel();
        let mut search = Search {
            query: "fresh".into(),
            ..Default::default()
        };
        search.jobs[Mode::Index as usize] = Some(Job {
            cancel: Arc::new(AtomicBool::new(true)),
            result: rx,
        });
        search.open(None);
        assert!(!search.can_start(None));
        search.open(Some(&Location::Local(root.path().to_path_buf())));
        search.start(None, &ctx);
        let results = search
            .job()
            .expect("folder search worker")
            .result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(paths(&results), [path]);
    }

    #[test]
    fn missing_index_result_is_removed_without_revealing_or_losing_other_hits() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let stale = root.path().join("removed-parent").join("indexed.txt");
        let valid = root.path().join("valid.txt");
        fs::write(&valid, "disposable file").unwrap();
        let (tx, result) = mpsc::channel();
        tx.send((Instant::now(), stale.symlink_metadata().map(|_| ())))
            .unwrap();
        let mut search = Search {
            mode: Mode::Index,
            visible: true,
            results: Some(Results {
                matches: vec![stale.clone().into(), valid.clone().into()],
                limited: false,
                warning: String::new(),
            }),
            selected: Some(0),
            index_reveal: Some(IndexReveal {
                path: Some(stale.clone()),
                started: Instant::now(),
                result,
            }),
            ..Default::default()
        };
        assert!(search.poll_index_reveal(&ctx).is_none());
        assert!(search.visible);
        assert!(search
            .error
            .as_ref()
            .unwrap()
            .contains(&*stale.to_string_lossy()));
        assert!(search.selected.is_none());
        assert_eq!(paths(search.results.as_ref().unwrap()), [valid]);
    }

    #[test]
    fn index_results_arrive_even_if_inspection_is_still_waiting() {
        let status = Rc::new(IndexStatus::new(None));
        let mut search = Search {
            mode: Mode::Index,
            index_status: Some(status.clone()),
            ..Default::default()
        };
        let (tx, result) = mpsc::channel();
        let (inspection_tx, inspection) = mpsc::channel();
        search.index_inspection = Some(IndexInspection {
            job: Job {
                cancel: Arc::new(AtomicBool::new(false)),
                result: inspection,
            },
            status: Rc::downgrade(&status),
        });
        search.jobs[Mode::Index as usize] = Some(Job {
            cancel: Arc::new(AtomicBool::new(false)),
            result,
        });
        let path = PathBuf::from("/indexed.txt");
        tx.send(Ok(Results {
            matches: vec![path.clone().into()],
            limited: false,
            warning: String::new(),
        }))
        .unwrap();

        search.poll();

        assert!(search.index_inspection.is_some());
        assert!(search.job().is_none());
        assert_eq!(paths(search.results.as_ref().unwrap()), [path]);

        let ctx = egui::Context::default();
        let fixture = tempfile::tempdir().unwrap();
        search.clear();
        let next_status = Rc::new(IndexStatus::new(Some(
            fixture.path().join("missing-locate"),
        )));
        search.index_status = Some(next_status.clone());
        search.poll_index_inspection(&ctx);
        assert!(search.index_inspection.as_ref().unwrap().job.cancelled());
        inspection_tx
            .send(IndexInfo {
                source: "cancelled index".into(),
                freshness: "cancelled date".into(),
            })
            .unwrap();
        search.poll_index_inspection(&ctx);
        assert_eq!(status.info.borrow().source, IndexInfo::unknown().source);
        assert!(next_status.pending.take().is_none());
        assert!(!search.index_inspection.as_ref().unwrap().job.cancelled());
    }

    #[test]
    fn late_index_metadata_follows_the_docked_results_during_a_new_search() {
        let ctx = egui::Context::default();
        let status = Rc::new(IndexStatus::new(None));
        let (sender, result) = mpsc::channel();
        let mut search = Search {
            mode: Mode::Index,
            index_inspection: Some(IndexInspection {
                job: Job {
                    cancel: Arc::new(AtomicBool::new(false)),
                    result,
                },
                status: Rc::downgrade(&status),
            }),
            index_status: Some(status),
            ..Default::default()
        };
        search.panel.replace(
            Results {
                matches: vec![PathBuf::from("/indexed.txt").into()],
                limited: false,
                warning: String::new(),
            },
            false,
            "indexed".into(),
            search.index_status.take(),
        );
        search.cancel_work();
        assert!(!search.index_inspection.as_ref().unwrap().job.cancelled());
        let next_status = Rc::new(IndexStatus::new(None));
        search.index_status = Some(next_status.clone());

        sender
            .send(IndexInfo {
                source: "docked index".into(),
                freshness: "docked date".into(),
            })
            .unwrap();
        search.poll_index_inspection(&ctx);

        let info = search.panel.index_status().unwrap().info.borrow();
        assert_eq!(info.source, "docked index");
        assert_eq!(info.freshness, "docked date");
        assert_eq!(
            next_status.info.borrow().source,
            IndexInfo::unknown().source
        );
    }

    #[test]
    fn timed_out_reveal_discards_late_success_and_releases_the_ui() {
        let ctx = egui::Context::default();
        let (tx, result) = mpsc::channel();
        let mut search = Search {
            index_reveal: Some(IndexReveal {
                path: Some(PathBuf::from("/indexed.txt")),
                started: Instant::now() - INDEX_REVEAL_WAIT,
                result,
            }),
            ..Default::default()
        };
        assert!(search.poll_index_reveal(&ctx).is_none());
        assert!(!search.checking_index_reveal());
        assert!(search.index_reveal.is_some());
        assert!(search.error.is_some());
        tx.send((Instant::now(), Ok(()))).unwrap();
        assert!(search.poll_index_reveal(&ctx).is_none());
        assert!(search.index_reveal.is_none());
    }

    #[test]
    fn folder_search_finds_fresh_unicode_names_and_prunes_exclusions() {
        let root = tempfile::tempdir().unwrap();
        for subdir in ["nested", "target", "cache-one"] {
            let folder = root.path().join(subdir);
            fs::create_dir(&folder).unwrap();
            fs::write(folder.join("CAFÉ ФАЙЛ.RS"), "").unwrap();
        }
        let folder = root.path().join("nested");
        fs::write(folder.join("CAFÉ ФАЙЛ.tmp"), "").unwrap();
        fs::write(folder.join("café файл.ignore.rs"), "").unwrap();
        let excluded = " TARGET , cache-*, *.TMP, *.ignore.rs";
        for query in ["cafe\u{301} файл", "caf? ф*.RS"] {
            let results =
                run_folder(root.path(), FolderTarget::Name, query, excluded, &|| false).unwrap();
            assert_eq!(paths(&results), [folder.join("CAFÉ ФАЙЛ.RS")]);
            assert!(results.warning.is_empty());
        }
        assert!(
            run_folder(root.path(), FolderTarget::Name, "target", "target", &|| {
                false
            })
            .unwrap()
            .matches
            .is_empty()
        );
    }

    #[test]
    fn folder_search_can_be_cancelled_during_a_scan() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..8 {
            fs::write(root.path().join(format!("file{index}")), "").unwrap();
        }
        let checks = Cell::new(0);
        let cancelled = || {
            checks.set(checks.get() + 1);
            checks.get() == 5
        };
        assert_eq!(
            run_folder(root.path(), FolderTarget::Name, "file", "", &cancelled)
                .err()
                .unwrap(),
            crate::i18n::text!("Поиск отменён")
        );
    }

    #[test]
    fn folder_search_bounds_matches_and_reports_depth_limit() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..=MAX_RESULTS {
            fs::write(root.path().join(format!("file{index}")), "").unwrap();
        }
        let results = run_folder(root.path(), FolderTarget::Name, "file", "", &|| false).unwrap();
        assert_eq!(results.matches.len(), MAX_RESULTS);
        assert!(results.limited);
        fs::remove_file(root.path().join("file0")).unwrap();
        assert!(
            !run_folder(root.path(), FolderTarget::Name, "file", "", &|| false)
                .unwrap()
                .limited
        );

        let nested = "d/".repeat(MAX_OPEN_DIRECTORIES);
        let deepest = root.path().join(nested);
        fs::create_dir_all(&deepest).unwrap();
        fs::write(deepest.join("match"), "").unwrap();
        let results = run_folder(root.path(), FolderTarget::Name, "match", "", &|| false).unwrap();
        assert!(results.matches.is_empty());
        assert!(!results.warning.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn folder_search_preserves_native_paths_without_following_directory_links() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("test-outside.txt"), "").unwrap();
        symlink(outside.path(), root.path().join("outside")).unwrap();
        symlink(root.path(), root.path().join("loop")).unwrap();
        let native = root.path().join("test-два слова\n.txt");
        fs::write(&native, "").unwrap();
        let results = run_folder(root.path(), FolderTarget::Name, "test-", "", &|| false).unwrap();
        assert_eq!(paths(&results), [native]);
        assert!(results.warning.is_empty());

        // Simulate a directory swapped for a link between listing and descent.
        let child = root.path().join("child");
        fs::create_dir(&child).unwrap();
        let mut directory = Directory::open(root.path().to_path_buf()).unwrap();
        let entry = std::iter::from_fn(|| directory.next_entry().unwrap())
            .find(|entry| entry.name == "child")
            .unwrap();
        assert_eq!(entry.kind.unwrap(), EntryKind::Directory);
        fs::remove_dir(&child).unwrap();
        symlink(outside.path(), &child).unwrap();
        assert!(directory.open_child(&entry.name).is_err());
    }

    #[test]
    fn content_search_finds_literal_and_regex_matches_with_line_context() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let path = nested.join("данные.txt");
        let contents = "before\r\nРусский C++ example\r\n lowercase c++\nC++ and C++ again\n";
        fs::write(&path, contents).unwrap();
        fs::write(nested.join("excluded.tmp"), contents).unwrap();
        fs::create_dir(root.path().join("ignored")).unwrap();
        fs::write(root.path().join("ignored/test.txt"), contents).unwrap();

        for (target, query, expected) in [
            (FolderTarget::Text, "C++", &[2, 4][..]),
            (FolderTarget::Regex, r"(?i)c\+{2}", &[2, 3, 4][..]),
        ] {
            let results =
                run_folder(root.path(), target, query, "*.tmp, ignored", &|| false).unwrap();
            assert!(results.warning.is_empty());
            let lines: Vec<_> = results
                .matches
                .iter()
                .map(|hit| {
                    assert_eq!(hit.path.path(), path);
                    hit.context.as_ref().unwrap().line
                })
                .collect();
            assert_eq!(lines, expected);
            assert_eq!(
                results.matches[0].context.as_ref().unwrap().excerpt,
                "Русский C++ example"
            );
        }
        assert!(
            run_folder(root.path(), FolderTarget::Regex, "[", "", &|| false)
                .err()
                .unwrap()
                .contains("regular expression")
        );
    }

    #[test]
    fn content_search_reports_skipped_files_and_bounds_matching_lines() {
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("binary.txt");
        fs::write(binary, b"hit\nend\0").unwrap();
        fs::write(root.path().join("invalid.txt"), b"hit\n\xff").unwrap();
        let oversized = fs::File::create(root.path().join("large.txt")).unwrap();
        oversized.set_len(content::MAX_FILE_BYTES + 1).unwrap();
        let results = run_folder(root.path(), FolderTarget::Text, "hit", "", &|| false).unwrap();
        assert!(results.matches.is_empty());
        assert!(results.warning.contains("larger than 1 MiB (1)"));
        assert!(results.warning.contains("non-UTF-8 files (2)"));

        let path = root.path().join("matches.txt");
        fs::write(&path, "hit\n".repeat(MAX_RESULTS + 1)).unwrap();
        let results = run_folder(root.path(), FolderTarget::Text, "hit", "", &|| false).unwrap();
        assert_eq!(results.matches.len(), MAX_RESULTS);
        assert_eq!(
            results.matches[MAX_RESULTS - 1]
                .context
                .as_ref()
                .unwrap()
                .line,
            MAX_RESULTS
        );
        assert!(results.limited);
    }

    #[cfg(unix)]
    #[test]
    fn content_search_does_not_open_links_or_special_files() {
        use std::{
            ffi::CString,
            os::unix::{ffi::OsStrExt, fs::symlink},
        };

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("external.txt");
        fs::write(&outside_file, "secret\n").unwrap();
        symlink(outside.path(), root.path().join("outside")).unwrap();
        symlink(&outside_file, root.path().join("symlink.txt")).unwrap();
        let swapped = root.path().join("swap.txt");
        fs::write(&swapped, "placeholder").unwrap();
        let mut directory = Directory::open(root.path().to_path_buf()).unwrap();
        let entry = std::iter::from_fn(|| directory.next_entry().unwrap())
            .find(|entry| entry.name == "swap.txt")
            .unwrap();
        assert_eq!(entry.kind.unwrap(), EntryKind::File);
        fs::remove_file(&swapped).unwrap();
        symlink(&outside_file, &swapped).unwrap();
        assert!(directory.open_file(&entry.name).unwrap().is_none());
        fs::remove_file(&swapped).unwrap();
        let fifo_path = CString::new(swapped.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_path is a NUL-terminated pathname in our temporary folder.
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        assert!(directory.open_file(&entry.name).unwrap().is_none());

        let results = run_folder(root.path(), FolderTarget::Text, "secret", "", &|| false).unwrap();
        assert!(results.matches.is_empty());
        assert!(results.warning.is_empty());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn system_bsd_locate_searches_a_disposable_index() {
        use std::io::Write;
        let (root, cli) =
            fake("exec /usr/bin/locate -d \"$(dirname \"$0\")/test.database\" \"$@\"");
        let database = std::fs::File::create(root.path().join("test.database")).unwrap();
        let mut builder = Command::new("/usr/libexec/locate.mklocatedb")
            .stdin(Stdio::piped())
            .stdout(database)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        builder
            .stdin
            .take()
            .unwrap()
            .write_all(b"/tmp/pawcommander-test/Two Words.RS\n/tmp/pawcommander-test/other.txt\n")
            .unwrap();
        let output = builder.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let results = run(&cli, "*.rs", &AtomicBool::new(false), TIMEOUT).unwrap();
        assert_eq!(
            paths(&results),
            [PathBuf::from("/tmp/pawcommander-test/Two Words.RS")]
        );
        assert!(run(&cli, "no-such-match", &AtomicBool::new(false), TIMEOUT)
            .unwrap()
            .matches
            .is_empty());
    }

    #[test]
    fn query_is_one_argument_after_option_terminator() {
        let cli = Path::new("locate");
        let query = "-d /tmp/db; $(touch oops) *.rs";
        let cmd = command(cli, query);
        let args: Vec<_> = cmd.get_args().map(|arg| arg.to_string_lossy()).collect();
        assert_eq!(args, ["-0", "-i", "-l", "1001", "--", query]);
    }

    #[test]
    fn output_reader_enforces_limit() {
        assert_eq!(read_capped(&b"abc"[..], 3).unwrap(), b"abc");
        assert!(read_capped(&b"abcd"[..], 3).is_err());
    }

    #[cfg(unix)]
    fn fake(script: &str) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let cli = root.path().join("fake locate");
        std::fs::write(&cli, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        (root, cli)
    }

    #[test]
    #[cfg(unix)]
    fn results_preserve_spaces_unicode_newlines_and_non_utf8() {
        let (_root, cli) = fake("printf '/tmp/two words\\000/tmp/файл\\000/tmp/line\\nbreak\\000/tmp/\\377\\000relative\\000'");
        let result = run(&cli, "*", &AtomicBool::new(false), TIMEOUT).unwrap();
        let paths = paths(&result);
        assert_eq!(paths.len(), 4);
        assert!(paths.contains(&PathBuf::from("/tmp/two words")));
        assert!(paths.contains(&PathBuf::from("/tmp/файл")));
        assert!(paths.contains(&PathBuf::from("/tmp/line\nbreak")));
    }

    #[test]
    #[cfg(unix)]
    fn empty_result_is_distinct_from_database_error_and_warning() {
        let (_root, cli) = fake("exit 1");
        assert!(run(&cli, "x", &AtomicBool::new(false), TIMEOUT)
            .unwrap()
            .matches
            .is_empty());
        let (_root, cli) = fake("echo 'database missing' >&2; exit 1");
        assert!(run(&cli, "x", &AtomicBool::new(false), TIMEOUT)
            .err()
            .unwrap()
            .contains("database missing"));
        let (_root, cli) = fake("echo 'database is stale' >&2; exit 0");
        assert_eq!(
            run(&cli, "x", &AtomicBool::new(false), TIMEOUT)
                .unwrap()
                .warning,
            "database is stale"
        );
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_and_timeout_stop_child() {
        // exec ensures the fixture owns its pipes, just as the locate process does.
        let (_root, cli) = fake("exec /bin/sleep 10");
        let start = Instant::now();
        assert!(run(&cli, "x", &AtomicBool::new(true), TIMEOUT)
            .err()
            .unwrap()
            .contains("Search cancelled"));
        assert!(run(
            &cli,
            "x",
            &AtomicBool::new(false),
            Duration::from_millis(30)
        )
        .is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    #[cfg(unix)]
    fn results_are_bounded() {
        let (_root, cli) = fake(
            "i=0; while [ \"$i\" -lt 1001 ]; do printf '/tmp/file%s\\000' \"$i\"; i=$((i+1)); done",
        );
        let result = run(&cli, "x", &AtomicBool::new(false), TIMEOUT).unwrap();
        assert_eq!(result.matches.len(), MAX_RESULTS);
        assert!(result.limited);
    }
}
