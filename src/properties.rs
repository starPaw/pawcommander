//! Read-only local properties. Cancellation is cooperative between filesystem calls.
use chrono::{DateTime, Local};
use eframe::egui;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

#[derive(Default, Clone, Debug)]
struct Summary {
    files: u64,
    directories: u64,
    links: u64,
    other: u64,
    bytes: u64,
    errors: u64,
    error_examples: Vec<String>,
    details: Vec<(PathBuf, String)>,
    done: bool,
    cancelled: bool,
}
impl Summary {
    fn error(&mut self, path: &Path, error: impl std::fmt::Display) {
        self.errors += 1;
        if self.error_examples.len() < 5 {
            self.error_examples
                .push(format!("{}: {error}", path.display()));
        }
    }
}

pub struct Properties {
    state: Arc<Mutex<Summary>>,
    cancel: Arc<AtomicBool>,
}
impl Drop for Properties {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
impl Properties {
    pub fn start(paths: Vec<PathBuf>, ctx: &egui::Context) -> Self {
        let state = Arc::new(Mutex::new(Summary::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_state = state.clone();
        let worker_cancel = cancel.clone();
        let ctx = ctx.clone();
        let result = std::thread::Builder::new()
            .name("pawcommander-properties".into())
            .spawn(move || {
                let mut last = Instant::now();
                let summary = scan(&paths, &worker_cancel, |summary| {
                    if last.elapsed() >= Duration::from_millis(100) {
                        *worker_state.lock().unwrap() = summary.clone();
                        ctx.request_repaint();
                        last = Instant::now();
                    }
                });
                *worker_state.lock().unwrap() = summary;
                ctx.request_repaint();
            });
        if let Err(error) = result {
            let mut summary = state.lock().unwrap();
            summary.error(Path::new(crate::i18n::text("Подсчёт не запущен")), error);
            summary.done = true;
        }
        Self { state, cancel }
    }

    /// Returns false after closing; dropping the window also cancels the worker.
    pub fn show(&self, ctx: &egui::Context) -> bool {
        let summary = self.state.lock().unwrap().clone();
        let mut open = true;
        egui::Window::new(crate::i18n::text("Свойства локальных файлов"))
            .id(egui::Id::new("local-properties"))
            .open(&mut open)
            .default_width(580.0)
            .show(ctx, |ui| {
                if !summary.done {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(if self.cancel.load(Ordering::Relaxed) {
                            crate::i18n::text("Остановка подсчёта…")
                        } else {
                            crate::i18n::text("Подсчёт…")
                        });
                        if ui.button(crate::i18n::text("Остановить подсчёт")).clicked() {
                            self.cancel.store(true, Ordering::Relaxed);
                        }
                    });
                } else {
                    ui.label(if summary.cancelled {
                        crate::i18n::text("Подсчёт остановлен — частичный результат")
                    } else if summary.errors > 0 {
                        crate::i18n::text("Подсчёт завершён с ошибками — частичный результат")
                    } else {
                        crate::i18n::text("Подсчёт завершён")
                    });
                }
                ui.label(crate::tr_format!("Размер файлов: {} байт", "File size: {} bytes", summary.bytes));
                ui.label(crate::tr_format!("Файлов: {} · Папок: {} · Ссылок: {} · Других: {}", "Files: {} · Folders: {} · Links: {} · Other: {}",
                    summary.files, summary.directories, summary.links, summary.other
                ));
                ui.small(crate::i18n::text("Включая выбранные объекты и скрытые файлы. Ссылки не раскрываются. Размер — сумма длин обычных файлов, не занятое место на диске."));
                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    for (path, details) in &summary.details {
                        ui.separator();
                        ui.label(path.display().to_string());
                        ui.label(details);
                    }
                    if summary.errors > 0 {
                        ui.separator();
                        ui.label(crate::tr_format!("Ошибок чтения: {}", "Read errors: {}", summary.errors));
                        for error in &summary.error_examples {
                            ui.label(error);
                        }
                    }
                });
            });
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            open = false;
        }
        open
    }
}

fn date(value: std::io::Result<std::time::SystemTime>) -> String {
    value.map_or_else(
        |_| crate::i18n::text("недоступно").into(),
        |time| {
            DateTime::<Local>::from(time)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        },
    )
}
fn details(metadata: &Metadata) -> String {
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{:04o} · UID {} · GID {}",
            metadata.mode() & 0o7777,
            metadata.uid(),
            metadata.gid()
        )
    };
    #[cfg(not(unix))]
    let permissions = crate::tr_format!(
        "Только чтение: {}",
        "Read-only: {}",
        metadata.permissions().readonly()
    );
    crate::tr_format!(
        "Права: {permissions}\nИзменён: {}\nСоздан: {}\nДоступ: {}",
        "Permissions: {permissions}\nModified: {}\nCreated: {}\nAccessed: {}",
        date(metadata.modified()),
        date(metadata.created()),
        date(metadata.accessed())
    )
}

fn scan(paths: &[PathBuf], cancel: &AtomicBool, mut progress: impl FnMut(&Summary)) -> Summary {
    let mut summary = Summary::default();
    // Sort ancestors first. A directory selected together with its descendants
    // counts once, without canonicalizing paths (which would follow symlinks).
    let mut roots = paths.to_vec();
    roots.sort();
    roots.dedup();
    let mut accepted: Vec<PathBuf> = Vec::new();
    for path in roots {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if accepted.iter().any(|root| path.starts_with(root)) {
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                summary.details.push((path.clone(), details(&metadata)));
                if metadata.is_dir() {
                    accepted.push(path.clone());
                }
            }
            Err(error) => {
                summary.error(&path, error);
                continue;
            }
        }
        let mut pending = vec![path];
        while let Some(path) = pending.pop() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() {
                        summary.links += 1;
                    } else if metadata.is_file() {
                        summary.files += 1;
                        summary.bytes = summary.bytes.saturating_add(metadata.len());
                    } else if metadata.is_dir() {
                        summary.directories += 1;
                        match fs::read_dir(&path) {
                            Ok(entries) => {
                                for entry in entries {
                                    if cancel.load(Ordering::Relaxed) {
                                        break;
                                    }
                                    match entry {
                                        Ok(entry) => pending.push(entry.path()),
                                        Err(error) => summary.error(&path, error),
                                    }
                                }
                            }
                            Err(error) => summary.error(&path, error),
                        }
                    } else {
                        summary.other += 1;
                    }
                }
                Err(error) => summary.error(&path, error),
            }
            progress(&summary);
        }
    }
    summary.cancelled = cancel.load(Ordering::Relaxed);
    summary.done = true;
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_group_hidden_files_and_empty_directories_without_overlap() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("папка");
        fs::create_dir_all(dir.join("empty")).unwrap();
        fs::write(dir.join(".hidden"), b"123").unwrap();
        let file = temp.path().join("other");
        fs::write(&file, b"12345").unwrap();
        let result = scan(
            &[dir.join(".hidden"), file.clone(), dir, file],
            &AtomicBool::new(false),
            |_| {},
        );
        assert_eq!((result.files, result.directories, result.bytes), (2, 2, 8));
        assert_eq!(result.details.len(), 2);
        assert!(result.done && !result.cancelled);
        assert_eq!(result.errors, 0);
    }

    #[test]
    fn cancellation_returns_partial_counts_and_stops_traversal() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..10 {
            fs::write(temp.path().join(index.to_string()), b"hello").unwrap();
        }
        let cancel = AtomicBool::new(false);
        let result = scan(&[temp.path().into()], &cancel, |_| {
            cancel.store(true, Ordering::Relaxed)
        });
        assert!(result.cancelled && result.done);
        assert_eq!(result.directories, 1);
        assert_eq!(result.files, 0);
        let result = scan(&[temp.path().into()], &cancel, |_| {
            panic!("already cancelled")
        });
        assert_eq!(result.directories, 0);
    }

    #[test]
    fn missing_roots_report_partial_result_with_bounded_error_examples() {
        let temp = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..12).map(|i| temp.path().join(i.to_string())).collect();
        let result = scan(&paths, &AtomicBool::new(false), |_| {});
        assert_eq!(result.errors, 12);
        assert_eq!(result.error_examples.len(), 5);
        assert!(result.done);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cycles_broken_links_and_external_targets_are_not_followed() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(temp.path().join("outside"), b"not counted").unwrap();
        fs::write(root.join("file"), b"yes").unwrap();
        fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&root, root.join("cycle")).unwrap();
        symlink(temp.path().join("outside"), root.join("external")).unwrap();
        symlink(root.join("absent"), root.join("broken")).unwrap();
        let result = scan(std::slice::from_ref(&root), &AtomicBool::new(false), |_| {});
        assert_eq!(
            (result.files, result.directories, result.links, result.bytes),
            (1, 1, 3, 3)
        );
        assert_eq!(result.errors, 0);
        let result = scan(&[root.join("file")], &AtomicBool::new(false), |_| {});
        assert!(result.details[0].1.contains("0640"));
        assert!(result.details[0].1.contains("Modified:"));
    }

    #[test]
    fn worker_publishes_counts_and_window_closes_on_escape() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("data"), b"1234567").unwrap();
        let ctx = egui::Context::default();
        let properties = Properties::start(vec![temp.path().into()], &ctx);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if properties.state.lock().unwrap().done {
                break;
            }
            assert!(Instant::now() < deadline, "worker did not complete");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(properties.state.lock().unwrap().bytes, 7);
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            assert!(properties.show(ctx))
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
        let _ = ctx.run(input, |ctx| assert!(!properties.show(ctx)));
    }

    #[test]
    fn dropping_window_requests_cancellation() {
        let properties = Properties::start(vec![], &egui::Context::default());
        let cancel = properties.cancel.clone();
        drop(properties);
        assert!(cancel.load(Ordering::Relaxed));
    }
}
