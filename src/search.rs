//! Live folder and indexed path search. Both run on a cancellable worker.
mod directory;

use crate::name_match::{normalize, Mask};
use directory::Directory;
use eframe::egui;
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
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

struct Results {
    paths: Vec<PathBuf>,
    limited: bool,
    warning: String,
}

struct Job {
    cancel: Arc<AtomicBool>,
    result: Receiver<Result<Results, String>>,
}

impl Job {
    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

impl Drop for Job {
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

#[derive(Default)]
pub struct Search {
    pub visible: bool,
    focus: bool,
    mode: Mode,
    folder: Option<PathBuf>,
    query: String,
    exclusions: String,
    jobs: [Option<Job>; 2],
    results: Option<Results>,
    selected: Option<PathBuf>,
    error: Option<String>,
}

impl Search {
    pub fn open(&mut self, folder: Option<&Path>) {
        if self.folder.as_deref() != folder {
            if self.mode == Mode::Folder {
                self.clear();
            }
            self.folder = folder.map(Path::to_path_buf);
        }
        self.visible = true;
        self.focus = true;
    }

    fn clear(&mut self) {
        for job in self.jobs.iter().flatten() {
            job.cancel();
        }
        self.results = None;
        self.selected = None;
        self.error = None;
    }

    fn job(&self) -> Option<&Job> {
        self.jobs[self.mode as usize].as_ref()
    }

    fn can_start(&self, cli: Option<&Path>) -> bool {
        self.job().is_none()
            && !self.query.trim().is_empty()
            && match self.mode {
                Mode::Folder => self.folder.is_some(),
                Mode::Index => cli.is_some(),
            }
    }

    fn start(&mut self, cli: Option<&Path>, ctx: &egui::Context) {
        if !self.can_start(cli) {
            return;
        }
        self.clear();
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let mode = self.mode;
        let folder = self.folder.clone();
        let cli = cli.map(Path::to_path_buf);
        let query = self.query.clone();
        let exclusions = self.exclusions.clone();
        let ctx = ctx.clone();
        let worker = std::thread::Builder::new().spawn(move || {
            let result = match mode {
                Mode::Folder => run_folder(
                    folder.as_deref().expect("validated folder"),
                    &query,
                    &exclusions,
                    &|| worker_cancel.load(Ordering::Relaxed),
                ),
                Mode::Index => run(
                    cli.as_deref().expect("validated locate tool"),
                    &query,
                    &worker_cancel,
                    TIMEOUT,
                ),
            };
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        match worker {
            Ok(_) => self.jobs[mode as usize] = Some(Job { cancel, result: rx }),
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
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(crate::i18n::text(
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

    pub fn show(&mut self, ctx: &egui::Context, cli: Option<&Path>) -> Option<PathBuf> {
        self.poll();
        if !self.visible {
            return None;
        }
        let mut visible = true;
        let mut reveal = None;
        egui::Window::new(crate::i18n::text("Поиск файлов"))
            .id(egui::Id::new("file_search"))
            .open(&mut visible)
            .default_width(720.0)
            .show(ctx, |ui| {
                let previous_mode = self.mode;
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.mode, Mode::Folder, crate::i18n::text("В папке и подпапках"));
                    ui.selectable_value(&mut self.mode, Mode::Index, crate::i18n::text("Индекс locate"));
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
                                    .hint_text(crate::i18n::text("Исключить имена через запятую, например .git, target, *.tmp"))
                                    .desired_width(ui.available_width()),
                            );
                            ctx.accesskit_node_builder(exclusions.id, |node| {
                                node.set_label(crate::i18n::text("Исключения поиска"));
                            });
                            if exclusions.changed() {
                                self.clear();
                            }
                            submit = exclusions.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        } else {
                            ui.label(crate::i18n::text("Для поиска в папке откройте локальную папку в панели."));
                        }
                        crate::i18n::text("Часть имени или маска: отчёт, *.rs, фото?.jpg")
                    }
                    Mode::Index => {
                        ui.label(crate::i18n::text("Поиск по полному пути в локальном индексе, без учёта регистра."));
                        ui.label(crate::i18n::text("База может отставать от файловой системы; SFTP не индексируется."));
                        if let Some(cli) = cli {
                            ui.small(format!("locate: {}", cli.display()));
                        } else {
                            ui.colored_label(ui.visuals().error_fg_color, crate::i18n::text("locate не найден"));
                            ui.label(crate::i18n::text("На Linux установите locate/plocate; на macOS используется /usr/bin/locate. Затем: Настройки → Инструменты → Проверить снова."));
                        }
                        crate::i18n::text("Часть пути или маска: Cargo, *.rs, */Documents/*")
                    }
                };
                ui.horizontal(|ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text(hint)
                            .desired_width((ui.available_width() - 175.0).max(120.0)),
                    );
                    ctx.accesskit_node_builder(response.id, |node| {
                        node.set_label(crate::i18n::text("Запрос поиска"));
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
                        egui::Button::new(crate::i18n::text("Найти")),
                    ).clicked();
                    if submit {
                        self.start(cli, ctx);
                    }
                    if let Some(job) = self.job() {
                        ui.spinner();
                        if job.cancelled() {
                            ui.label(crate::i18n::text("Остановка поиска…"));
                        } else if ui.button(crate::i18n::text("Отмена")).clicked() {
                            job.cancel();
                            self.error = Some(crate::i18n::text("Поиск отменён").into());
                        }
                    }
                });
                if let Some(error) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                if self.mode == Mode::Index {
                    ui.small(crate::i18n::text("Если база отсутствует или устарела, обновите её средствами ОС (locate.updatedb / updatedb)."));
                }
                if let Some(results) = &self.results {
                    ui.separator();
                    ui.label(crate::tr_format!("Найдено: {}{}", "Found: {}{}", results.paths.len(), if results.limited { crate::i18n::text("+ · Уточните запрос, показаны первые 1000") } else { "" }));
                    if !results.warning.is_empty() {
                        ui.colored_label(ui.visuals().warn_fg_color, &results.warning);
                    }
                    if results.paths.is_empty() {
                        ui.label(match self.mode {
                            Mode::Folder => crate::i18n::text("В просмотренных папках совпадений нет."),
                            Mode::Index => crate::i18n::text("Совпадений в индексе нет. Проверьте запрос и актуальность базы locate."),
                        });
                    }
                    egui::ScrollArea::vertical()
                        .id_salt("search_results")
                        .max_height(340.0)
                        .show_rows(ui, 28.0, results.paths.len(), |ui, rows| {
                            for index in rows {
                                let path = &results.paths[index];
                                let response = ui.add(
                                    egui::Button::new(path.display().to_string())
                                        .selected(self.selected.as_ref() == Some(path))
                                        .truncate()
                                        .min_size(egui::vec2(ui.available_width(), 28.0)),
                                ).on_hover_text(path.display().to_string());
                                if response.clicked() {
                                    self.selected = Some(path.clone());
                                }
                                if response.double_clicked() {
                                    reveal = Some(path.clone());
                                }
                            }
                        });
                    if ui.add_enabled(self.selected.is_some(), egui::Button::new(crate::i18n::text("Показать в панели"))).clicked() {
                        reveal = self.selected.clone();
                    }
                    ui.small(crate::i18n::text("Двойной щелчок — перейти к результату в активной панели."));
                }
            });
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            visible = false;
        }
        self.visible = visible && reveal.is_none();
        if !self.visible {
            for job in self.jobs.iter().flatten() {
                job.cancel();
            }
        } else if self.job().is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        reveal
    }
}

fn run_folder(
    folder: &Path,
    query: &str,
    exclusions: &str,
    cancelled: &impl Fn() -> bool,
) -> Result<Results, String> {
    let cancellation = || crate::i18n::text("Поиск отменён").into();
    if cancelled() {
        return Err(cancellation());
    }
    if query.trim().is_empty() || query.contains('\0') {
        return Err(crate::i18n::text("Введите непустой запрос без NUL").into());
    }
    if query.len() > MAX_PATTERN_BYTES
        || exclusions.len() > MAX_PATTERN_BYTES
        || exclusions.contains('\0')
    {
        return Err(crate::i18n::text("Укоротите запрос или список исключений.").into());
    }
    let query = normalize(query);
    let mask = query.contains(['*', '?']).then(|| Mask::new(&query));
    let exclusions = exclusions
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| Mask::new(&normalize(name)))
        .collect::<Vec<_>>();

    let directory = Directory::open(folder.to_path_buf()).map_err(|error| {
        crate::tr_format!(
            "Не удалось прочитать папку: {error}",
            "Could not read folder: {error}"
        )
    })?;
    let mut directories = vec![directory];
    let mut paths = Vec::new();
    let mut limited = false;
    let mut depth_limited = false;
    let mut unreadable = 0;
    loop {
        if cancelled() {
            return Err(cancellation());
        }
        let at_depth_limit = directories.len() == MAX_OPEN_DIRECTORIES;
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
        let name = entry.name.to_string_lossy();
        let normalized = normalize(&name);
        if exclusions.iter().any(|mask| mask.matches(&normalized)) {
            continue;
        }
        let matches = match &mask {
            Some(mask) => mask.matches(&normalized),
            None => normalized.contains(query.as_ref()),
        };
        if matches {
            if paths.len() == MAX_RESULTS {
                limited = true;
                break;
            }
            paths.push(directory.path.join(&entry.name));
        }
        match entry.descendable {
            Ok(false) => {}
            Ok(_) if at_depth_limit => depth_limited = true,
            Ok(_) => match directory.open_child(&entry.name) {
                Ok(Some(directory)) => directories.push(directory),
                Ok(None) => {}
                Err(_) => unreadable += 1,
            },
            Err(_) => unreadable += 1,
        }
    }
    paths.sort_unstable();
    let mut warnings = Vec::new();
    if unreadable > 0 {
        warnings.push(crate::tr_format!(
            "Не удалось прочитать некоторые файлы или папки ({unreadable}).",
            "Some files or folders could not be read ({unreadable})."
        ));
    }
    if depth_limited {
        warnings.push(crate::i18n::text("Поиск пропустил слишком глубоко вложенные папки.").into());
    }
    Ok(Results {
        paths,
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
        return Err(io::Error::other(crate::i18n::text(
            "Превышен лимит вывода locate; уточните запрос",
        )));
    }
    Ok(bytes)
}

fn run(cli: &Path, query: &str, cancel: &AtomicBool, timeout: Duration) -> Result<Results, String> {
    if query.trim().is_empty() || query.contains('\0') {
        return Err(crate::i18n::text("Введите непустой запрос без NUL").into());
    }
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
            break Err(crate::i18n::text("Поиск отменён").to_string());
        }
        if started.elapsed() >= timeout {
            break Err(
                crate::i18n::text("locate не ответил за 15 секунд; проверьте базу индекса")
                    .to_string(),
            );
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
        .map_err(|_| crate::i18n::text("Ошибка чтения вывода locate"))?;
    let stderr = err
        .join()
        .map_err(|_| crate::i18n::text("Ошибка чтения ошибок locate"))?;
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
        paths,
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
            .map_err(|_| crate::i18n::text("locate вернул путь не в UTF-8").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, fs};

    #[test]
    fn closing_search_cancels_work_and_missing_tool_does_not_start_it() {
        let ctx = egui::Context::default();
        let mut search = Search {
            query: "Cargo".into(),
            mode: Mode::Index,
            ..Default::default()
        };
        search.open(None);
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
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
        let _ = ctx.run(input, |ctx| {
            search.show(ctx, None);
        });
        assert!(!search.visible);
        assert!(cancel.load(Ordering::Relaxed));
        tx.send(Ok(Results {
            paths: vec![PathBuf::from("/stale")],
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
        search.open(Some(root.path()));
        search.start(None, &ctx);
        let results = search
            .job()
            .expect("folder search worker")
            .result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(results.paths, [path]);
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
            let results = run_folder(root.path(), query, excluded, &|| false).unwrap();
            assert_eq!(results.paths, [folder.join("CAFÉ ФАЙЛ.RS")]);
            assert!(results.warning.is_empty());
        }
        assert!(run_folder(root.path(), "target", "target", &|| false)
            .unwrap()
            .paths
            .is_empty());
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
            run_folder(root.path(), "file", "", &cancelled)
                .err()
                .unwrap(),
            crate::i18n::text("Поиск отменён")
        );
    }

    #[test]
    fn folder_search_bounds_matches_and_reports_depth_limit() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..=MAX_RESULTS {
            fs::write(root.path().join(format!("file{index}")), "").unwrap();
        }
        let results = run_folder(root.path(), "file", "", &|| false).unwrap();
        assert_eq!(results.paths.len(), MAX_RESULTS);
        assert!(results.limited);
        fs::remove_file(root.path().join("file0")).unwrap();
        assert!(
            !run_folder(root.path(), "file", "", &|| false)
                .unwrap()
                .limited
        );

        let nested = "d/".repeat(MAX_OPEN_DIRECTORIES);
        let deepest = root.path().join(nested);
        fs::create_dir_all(&deepest).unwrap();
        fs::write(deepest.join("match"), "").unwrap();
        let results = run_folder(root.path(), "match", "", &|| false).unwrap();
        assert!(results.paths.is_empty());
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
        let results = run_folder(root.path(), "test-", "", &|| false).unwrap();
        assert_eq!(results.paths, [native]);
        assert!(results.warning.is_empty());

        // Simulate a directory swapped for a link between listing and descent.
        let child = root.path().join("child");
        fs::create_dir(&child).unwrap();
        let mut directory = Directory::open(root.path().to_path_buf()).unwrap();
        let entry = std::iter::from_fn(|| directory.next_entry().unwrap())
            .find(|entry| entry.name == "child")
            .unwrap();
        assert!(entry.descendable.unwrap());
        fs::remove_dir(&child).unwrap();
        symlink(outside.path(), &child).unwrap();
        assert!(directory.open_child(&entry.name).is_err());
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
            results.paths,
            [PathBuf::from("/tmp/pawcommander-test/Two Words.RS")]
        );
        assert!(run(&cli, "no-such-match", &AtomicBool::new(false), TIMEOUT)
            .unwrap()
            .paths
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
        assert_eq!(result.paths.len(), 4);
        assert!(result.paths.contains(&PathBuf::from("/tmp/two words")));
        assert!(result.paths.contains(&PathBuf::from("/tmp/файл")));
        assert!(result.paths.contains(&PathBuf::from("/tmp/line\nbreak")));
    }

    #[test]
    #[cfg(unix)]
    fn empty_result_is_distinct_from_database_error_and_warning() {
        let (_root, cli) = fake("exit 1");
        assert!(run(&cli, "x", &AtomicBool::new(false), TIMEOUT)
            .unwrap()
            .paths
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
        assert_eq!(result.paths.len(), MAX_RESULTS);
        assert!(result.limited);
    }
}
