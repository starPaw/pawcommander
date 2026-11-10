//! Bounded, cancellable local index search. Paths are never passed to a shell.
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

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub struct Search {
    pub visible: bool,
    focus: bool,
    query: String,
    job: Option<Job>,
    results: Option<Results>,
    selected: Option<PathBuf>,
    error: Option<String>,
}

impl Search {
    pub fn open(&mut self) {
        self.visible = true;
        self.focus = true;
    }

    fn start(&mut self, cli: &Path, ctx: &egui::Context) {
        if self.query.trim().is_empty() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        self.job = Some(Job {
            cancel: cancel.clone(),
            result: rx,
        });
        self.results = None;
        self.selected = None;
        self.error = None;
        let cli = cli.to_path_buf();
        let query = self.query.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(run(&cli, &query, &cancel, TIMEOUT));
            ctx.request_repaint();
        });
    }

    pub fn show(&mut self, ctx: &egui::Context, cli: Option<&Path>) -> Option<PathBuf> {
        if !self.visible {
            return None;
        }
        if let Some(job) = &self.job {
            let result = match job.result.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(crate::i18n::text(
                    "Задача поиска завершилась неожиданно",
                )
                .into())),
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                match result {
                    Ok(results) => self.results = Some(results),
                    Err(error) => self.error = Some(error),
                }
                self.job = None;
            }
        }
        let mut visible = true;
        let mut reveal = None;
        egui::Window::new(crate::i18n::text("Суперпоиск · locate"))
            .open(&mut visible)
            .default_width(720.0)
            .show(ctx, |ui| {
                ui.label(crate::i18n::text("Поиск по полному пути в локальном индексе, без учёта регистра."));
                ui.label(crate::i18n::text("База может отставать от файловой системы; SFTP не индексируется."));
                if let Some(cli) = cli {
                    ui.small(format!("locate: {}", cli.display()));
                } else {
                    ui.colored_label(ui.visuals().error_fg_color, crate::i18n::text("locate не найден"));
                    ui.label(crate::i18n::text("На Linux установите locate/plocate; на macOS используется /usr/bin/locate. Затем: Настройки → Инструменты → Проверить снова."));
                }
                ui.horizontal(|ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text(crate::i18n::text("Часть пути или маска: Cargo, *.rs, */Documents/*"))
                            .desired_width((ui.available_width() - 175.0).max(120.0)),
                    );
                    if self.focus {
                        response.request_focus();
                        self.focus = false;
                    }
                    let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let search = ui.add_enabled(
                        cli.is_some() && !self.query.trim().is_empty() && self.job.is_none(),
                        egui::Button::new(crate::i18n::text("Найти")),
                    ).clicked();
                    if (search || enter) && self.job.is_none() {
                        if let Some(cli) = cli {
                            self.start(cli, ctx);
                        }
                    }
                    if self.job.is_some() {
                        ui.spinner();
                        if ui.button(crate::i18n::text("Отмена")).clicked() {
                            self.job = None;
                            self.error = Some(crate::i18n::text("Поиск отменён").into());
                        }
                    }
                });
                if let Some(error) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                ui.small(crate::i18n::text("Если база отсутствует или устарела, обновите её средствами ОС (locate.updatedb / updatedb)."));
                if let Some(results) = &self.results {
                    ui.separator();
                    ui.label(crate::tr_format!("Найдено: {}{}", "Found: {}{}", results.paths.len(), if results.limited { crate::i18n::text("+ · Уточните запрос, показаны первые 1000") } else { "" }));
                    if !results.warning.is_empty() {
                        ui.colored_label(ui.visuals().warn_fg_color, &results.warning);
                    }
                    if results.paths.is_empty() {
                        ui.label(crate::i18n::text("Совпадений в индексе нет. Проверьте запрос и актуальность базы locate."));
                    }
                    egui::ScrollArea::vertical()
                        .id_salt("locate_results")
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
            self.job = None;
        } else if self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        reveal
    }
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

    #[test]
    fn closing_search_cancels_work_and_missing_tool_does_not_start_it() {
        let ctx = egui::Context::default();
        let mut search = Search {
            query: "Cargo".into(),
            ..Default::default()
        };
        search.open();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            search.show(ctx, None);
        });
        assert!(search.visible);
        assert!(search.job.is_none());
        let cancel = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = mpsc::channel();
        search.job = Some(Job {
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
        assert!(search.job.is_none());
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
