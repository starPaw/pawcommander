//! Read-only local properties. Cancellation is cooperative between filesystem calls.
use chrono::{DateTime, Local};
use eframe::egui;
use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod unix;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use unix::scan_root;

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
    fn count(&mut self, metadata: &Metadata) {
        if metadata.is_symlink() {
            self.links += 1;
        } else if metadata.is_file() {
            self.files += 1;
            self.bytes = self.bytes.saturating_add(metadata.len());
        } else if metadata.is_dir() {
            self.directories += 1;
        } else {
            self.other += 1;
        }
    }

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
    let mut accepted = HashSet::new();
    for path in roots {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if path
            .ancestors()
            .skip(1)
            .any(|parent| accepted.contains(parent))
        {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                summary.error(&path, error);
                progress(&summary);
                continue;
            }
        };
        summary.details.push((path.clone(), details(&metadata)));
        if metadata.is_dir() {
            accepted.insert(path.clone());
        }

        scan_root(path, metadata, cancel, &mut summary, &mut progress);
    }
    summary.cancelled = cancel.load(Ordering::Relaxed);
    summary.done = true;
    summary
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn scan_root(
    path: PathBuf,
    metadata: Metadata,
    cancel: &AtomicBool,
    summary: &mut Summary,
    progress: &mut impl FnMut(&Summary),
) {
    let mut root_metadata = Some(metadata);
    // Hold one directory iterator at a time. Count ordinary entries while
    // reading them and only queue subdirectories for later descent.
    let mut pending = vec![path];
    while let Some(path) = pending.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        // Recheck queued directories before opening them in case an entry
        // has been replaced since it was read from its parent.
        let metadata = match root_metadata
            .take()
            .map_or_else(|| fs::symlink_metadata(&path), Ok)
        {
            Ok(metadata) => metadata,
            Err(error) => {
                summary.error(&path, error);
                progress(summary);
                continue;
            }
        };
        summary.count(&metadata);
        if !metadata.is_dir() || cancel.load(Ordering::Relaxed) {
            progress(summary);
            continue;
        }
        let mut entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) => {
                summary.error(&path, error);
                progress(summary);
                continue;
            }
        };
        progress(summary);
        while !cancel.load(Ordering::Relaxed) {
            match entries.next() {
                // DirEntry::metadata reports the link itself.
                Some(Ok(entry)) => match entry.metadata() {
                    Ok(metadata) if metadata.is_dir() => pending.push(entry.path()),
                    Ok(metadata) => summary.count(&metadata),
                    Err(error) => summary.error(&entry.path(), error),
                },
                Some(Err(error)) => summary.error(&path, error),
                None => break,
            }
            progress(summary);
        }
    }
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
    fn flat_folder_publishes_file_counts_before_cancellation() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..64 {
            fs::write(temp.path().join(index.to_string()), b"data").unwrap();
        }
        let cancel = AtomicBool::new(false);
        let mut first_file_progress = None;
        let result = scan(&[temp.path().into()], &cancel, |summary| {
            if summary.files > 0 {
                first_file_progress.get_or_insert((summary.files, summary.bytes));
                cancel.store(true, Ordering::Relaxed);
            }
        });

        assert_eq!(first_file_progress, Some((1, 4)));
        assert_eq!((result.files, result.directories, result.bytes), (1, 1, 4));
        assert!(result.done && result.cancelled);
        assert_eq!(result.errors, 0);
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

    #[cfg(unix)]
    #[test]
    fn queued_directory_replaced_by_symlink_is_counted_as_link() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let child = root.join("child");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(child.join("file"), b"original").unwrap();
        fs::write(outside.join("file"), b"outside").unwrap();

        let mut updates = 0;
        let result = scan(&[root], &AtomicBool::new(false), |_| {
            updates += 1;
            if updates == 2 {
                fs::rename(&child, temp.path().join("moved")).unwrap();
                symlink(&outside, &child).unwrap();
            }
        });
        assert_eq!(
            (result.files, result.directories, result.links, result.bytes),
            (0, 1, 1, 0)
        );
        assert_eq!(result.errors, 0);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn replacing_an_open_ancestor_keeps_the_scan_in_the_original_directory() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let ancestor = root.join("ancestor");
        let outside = temp.path().join("outside");
        fs::create_dir_all(ancestor.join("nested")).unwrap();
        fs::create_dir_all(outside.join("nested")).unwrap();
        fs::write(ancestor.join("nested/file"), b"yes").unwrap();
        fs::write(outside.join("nested/file"), [0; 101]).unwrap();

        let mut ancestor_updates = 0;
        let result = scan(&[root], &AtomicBool::new(false), |summary| {
            if summary.directories == 2 {
                ancestor_updates += 1;
                // The ancestor was opened on the first update. The next one
                // observed its nested directory and has yet to open it.
                if ancestor_updates == 2 {
                    fs::rename(&ancestor, temp.path().join("moved")).unwrap();
                    symlink(&outside, &ancestor).unwrap();
                }
            }
        });
        assert_eq!(ancestor_updates, 2);
        assert_eq!(
            (result.files, result.directories, result.links, result.bytes),
            (1, 3, 0, 3)
        );
        assert_eq!(result.errors, 0);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn replaced_child_directory_reports_partial_result_without_counting_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let child = root.join("child");
        fs::create_dir_all(&child).unwrap();
        fs::write(child.join("file"), b"original").unwrap();

        let mut updates = 0;
        let result = scan(&[root], &AtomicBool::new(false), |_| {
            updates += 1;
            if updates == 2 {
                fs::rename(&child, temp.path().join("moved")).unwrap();
                fs::create_dir(&child).unwrap();
                fs::write(child.join("file"), b"replacement").unwrap();
            }
        });
        assert!(result.done && !result.cancelled);
        assert_eq!((result.files, result.directories, result.bytes), (0, 1, 0));
        assert_eq!(result.errors, 1);
        assert!(result.error_examples[0].contains(&child.display().to_string()));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn cancellation_after_observing_a_subdirectory_skips_descent() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        fs::write(child.join("file"), b"data").unwrap();
        let cancel = AtomicBool::new(false);
        let mut updates = 0;
        let result = scan(&[temp.path().into()], &cancel, |_| {
            updates += 1;
            if updates == 2 {
                cancel.store(true, Ordering::Relaxed);
            }
        });

        assert!(result.done && result.cancelled);
        assert_eq!((result.files, result.directories, result.bytes), (0, 1, 0));
        assert_eq!(result.errors, 0);
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
