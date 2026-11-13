//! Conservative crash detection for operations that reached their worker.
//!
//! All I/O belongs on background workers. Records are descriptions, never
//! executable plans: recovery does not retry operations, inspect destinations,
//! discover staging directories, or remove any source, target, or staging data.
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

const VERSION: u32 = 1;
const MAX_BYTES: usize = 128 * 1024;
const MAX_ENTRIES: usize = 4096;
const PREFIX: &str = "operation-";
const SUFFIX: &str = ".json";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Record {
    pub label: String,
    pub description: String,
    pub started: u64,
}

#[derive(Deserialize, Serialize)]
struct Saved {
    version: u32,
    record: Record,
}

#[derive(Default)]
pub(crate) struct Report {
    pub unfinished: Vec<Record>,
    pub errors: Vec<String>,
}

/// Startup discovery and presentation; disk access stays on its scan worker.
#[derive(Default)]
pub(crate) struct Recovery {
    report: Report,
    receive: Option<Receiver<Report>>,
    directory: Option<PathBuf>,
    open: bool,
}

impl Recovery {
    pub fn new(ctx: &egui::Context) -> Self {
        // Ordinary app unit tests must not read or write the user's profile.
        if cfg!(test) {
            return Self::default();
        }
        let Some(root) = dirs::data_local_dir() else {
            return Self {
                report: Report {
                    errors: vec!["No local application-data directory".into()],
                    ..Report::default()
                },
                open: true,
                ..Self::default()
            };
        };
        Self::open_directory(root.join("PawCommander").join("recovery"), ctx)
    }

    #[cfg(test)]
    pub(crate) fn for_test(directory: PathBuf, ctx: &egui::Context) -> Self {
        Self::open_directory(directory, ctx)
    }

    fn open_directory(directory: PathBuf, ctx: &egui::Context) -> Self {
        let (send, receive) = mpsc::channel();
        let worker_directory = directory.clone();
        let worker_ctx = ctx.clone();
        let worker = std::thread::Builder::new()
            .name("pawcommander-recovery-scan".into())
            .spawn(move || {
                let _ = send.send(scan(&worker_directory));
                worker_ctx.request_repaint();
            });
        match worker {
            Ok(_) => Self {
                directory: Some(directory),
                receive: Some(receive),
                ..Self::default()
            },
            Err(error) => Self {
                directory: Some(directory),
                report: Report {
                    errors: vec![error.to_string()],
                    ..Report::default()
                },
                open: true,
                ..Self::default()
            },
        }
    }

    pub fn directory(&self) -> Option<PathBuf> {
        self.directory.clone()
    }

    /// Poll before handling keyboard shortcuts, since findings open the window.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let Some(receive) = &self.receive else {
            return;
        };
        match receive.try_recv() {
            Ok(report) => {
                self.open |= !report.unfinished.is_empty() || !report.errors.is_empty();
                self.report = report;
                self.receive = None;
                ctx.request_repaint();
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.report
                    .errors
                    .push("Recovery scan worker disconnected".into());
                self.receive = None;
                self.open = true;
                ctx.request_repaint();
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    pub fn reopen(&mut self) {
        self.open = true;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        egui::Window::new(crate::tr_format!("Восстановление после сбоя", "Crash recovery"))
            .id(egui::Id::new("crash_recovery"))
            .open(&mut self.open)
            .default_size([780.0, 440.0])
            .show(ctx, |ui| {
                ui.label(crate::tr_format!(
                    "Обнаружены задания без подтверждённого завершения. Некоторые файлы могли быть обработаны. Сверьте обе папки и журнал результатов перед повтором.",
                    "These operations have no confirmed completion. Some files may have been processed. Check both folders and result history before retrying."
                ));
                ui.label(crate::tr_format!(
                    "Это отчёт запуска. Ожидающие задания и временные копии пока не отслеживаются. Автоматический повтор и удаление файлов не выполняются.",
                    "This is a startup report. Queued jobs and temporary copies are not tracked yet. No automatic retry or file removal is performed."
                ));
                if let Some(directory) = &self.directory {
                    ui.add(egui::Label::new(directory.display().to_string()).selectable(true).wrap());
                }
                if !self.report.errors.is_empty() {
                    ui.colored_label(ui.visuals().error_fg_color, crate::tr_format!(
                        "Не все записи восстановления удалось прочитать. Существующие записи сохранены.",
                        "Some recovery records could not be read. Existing records were preserved."
                    ));
                }
                if self.receive.is_some() {
                    ui.label(crate::tr_format!("Проверка незавершённых заданий…", "Checking unfinished operations…"));
                } else if self.report.unfinished.is_empty() && self.report.errors.is_empty() {
                    ui.label(crate::tr_format!("Незавершённые задания не обнаружены.", "No unfinished operations found."));
                }
                egui::ScrollArea::both().show(ui, |ui| {
                    for error in &self.report.errors {
                        ui.add(egui::Label::new(error).selectable(true).wrap());
                    }
                    for record in &self.report.unfinished {
                        let time = chrono::DateTime::from_timestamp(record.started as i64, 0)
                            .map(|time| time.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
                            .unwrap_or_else(|| record.started.to_string());
                        ui.separator();
                        ui.add(egui::Label::new(format!("{time} · {}", record.label)).selectable(true).wrap());
                        ui.add(egui::Label::new(&record.description).selectable(true).wrap());
                    }
                });
            });
    }
}

/// The lock identifies a live worker without relying on reusable process IDs.
/// Dropping this handle deliberately preserves its marker, including on panic.
/// Only explicit completion removes the marker created by this handle.
pub(crate) struct PendingOperation {
    file: File,
    path: PathBuf,
}

/// Persist before executing the operation. An error means the caller must not
/// start work. The caller supplies an application-owned recovery directory.
pub(crate) fn begin(
    directory: &Path,
    label: &str,
    description: &str,
    started: u64,
) -> io::Result<PendingOperation> {
    // Bound input before serialization as escaping can amplify its size.
    if label.len().saturating_add(description.len()) > MAX_BYTES {
        return Err(io::Error::other("Recovery description exceeds 128 KiB"));
    }
    let saved = Saved {
        version: VERSION,
        record: Record {
            label: label.into(),
            description: description.into(),
            started,
        },
    };
    let bytes = serde_json::to_vec(&saved).map_err(io::Error::other)?;
    if bytes.len() > MAX_BYTES {
        return Err(io::Error::other("Recovery record exceeds 128 KiB"));
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(PREFIX)
        .suffix(".pending")
        .tempfile_in(directory)?;
    temporary.as_file().try_lock().map_err(io::Error::other)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    // Publish only a complete, already locked record. A concurrent startup scan
    // must never mistake a writer's partly serialized marker for corrupt data.
    let path = temporary.path().with_extension("json");
    let file = temporary
        .persist_noclobber(&path)
        .map_err(|error| error.error)?;
    // The marker remains conservative evidence if syncing its directory fails.
    sync_directory(directory)?;
    Ok(PendingOperation { file, path })
}

impl PendingOperation {
    /// Call only after a normal worker return and successful result persistence.
    /// A caught panic or failed history save must drop the handle instead.
    pub fn complete(self) -> io::Result<()> {
        // Retain the lock through deletion so a scanner cannot report a worker
        // in the small interval between releasing the handle and removal.
        if !same_owned_marker(&self.file, &self.path)? {
            return Err(io::Error::other(
                "Recovery marker identity changed; file preserved",
            ));
        }
        // Identity is checked immediately before unlinking. The application data
        // directory must remain private: this is not an atomic compare-and-unlink
        // against hostile same-user changes between these two system calls.
        fs::remove_file(&self.path)?;
        let result = sync_directory(
            self.path
                .parent()
                .ok_or_else(|| io::Error::other("Missing recovery directory"))?,
        );
        drop(self.file);
        result
    }
}

fn same_owned_marker(owned: &File, path: &Path) -> io::Result<bool> {
    let candidate = fs::symlink_metadata(path)?;
    if !candidate.file_type().is_file() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let original = owned.metadata()?;
        Ok(original.dev() == candidate.dev() && original.ino() == candidate.ino())
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            FileIdInfo, GetFileInformationByHandleEx, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
        };
        let candidate = OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        if candidate.metadata()?.file_type().is_symlink() {
            return Ok(false);
        }
        let file_id = |file: &File| {
            let mut info = FILE_ID_INFO::default();
            // SAFETY: the live file handle and output buffer match FileIdInfo.
            let result = unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileIdInfo,
                    (&raw mut info).cast(),
                    size_of::<FILE_ID_INFO>() as u32,
                )
            };
            let id = info.FileId.Identifier;
            (result != 0 && id != [0; 16] && id != [u8::MAX; 16])
                .then_some((info.VolumeSerialNumber, id))
        };
        Ok(file_id(owned).is_some_and(|id| file_id(&candidate) == Some(id)))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = owned;
        Ok(false)
    }
}

/// Read only this directory's bounded operation markers. A leftover record is
/// evidence that completion was not confirmed, not proof that nothing copied.
/// Invalid/unsupported markers are reported and preserved. Locked records are
/// active in another worker/instance and are omitted from the recovery report.
pub(crate) fn scan(directory: &Path) -> Report {
    let mut report = Report::default();
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return report,
        Err(error) => {
            report.errors.push(error.to_string());
            return report;
        }
    };
    let mut report_bytes = 0usize;
    for (index, entry) in entries.enumerate() {
        if index == MAX_ENTRIES {
            report.errors.push(format!(
                "Recovery scan stopped after {MAX_ENTRIES} entries; additional records may exist"
            ));
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.errors.push(error.to_string());
                continue;
            }
        };
        if !is_marker_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        match read_record(&path) {
            Ok(Some(record)) => {
                report_bytes = report_bytes
                    .saturating_add(record.label.len())
                    .saturating_add(record.description.len());
                if report_bytes > 8 * 1024 * 1024 {
                    report
                        .errors
                        .push("Recovery report exceeds 8 MiB; additional records may exist".into());
                    break;
                }
                report.unfinished.push(record);
            }
            Ok(None) => {}
            Err(error) => report.errors.push(format!("{}: {error}", path.display())),
        }
    }
    report.unfinished.sort_by_key(|record| record.started);
    report
}

fn is_marker_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(PREFIX))
        .and_then(|name| name.strip_suffix(SUFFIX))
        .is_some_and(|random| {
            !random.is_empty() && random.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
}

fn read_record(path: &Path) -> io::Result<Option<Record>> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(io::Error::other("Recovery marker is not a regular file"));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Do not follow a swapped symlink or block on a swapped FIFO.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::other("Recovery marker is not a regular file"));
    }
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(error)) => return Err(error),
    }
    let mut bytes = Vec::new();
    (&file).take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        return Err(io::Error::other("Recovery record exceeds 128 KiB"));
    }
    let saved: Saved = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if saved.version != VERSION {
        return Err(io::Error::other("Unsupported recovery record version"));
    }
    Ok(Some(saved.record))
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    #[ignore = "subprocess entry point for killed_worker_is_recovered"]
    fn crash_process_helper() {
        let directory =
            PathBuf::from(std::env::var_os("PAWCOMMANDER_RECOVERY_CRASH_TEST_DIR").unwrap());
        let _pending = begin(&directory, "copy", "source -> target", 42).unwrap();
        fs::write(directory.parent().unwrap().join("ready"), b"ready").unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    #[cfg(unix)]
    fn killed_worker_is_recovered() {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("recovery");
        let foreign = root.path().join(".paw-copy-foreign");
        fs::write(&foreign, b"foreign data survives process crash").unwrap();
        let mut child = ChildGuard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "recovery::tests::crash_process_helper",
                ])
                .env("PAWCOMMANDER_RECOVERY_CRASH_TEST_DIR", &directory)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !root.path().join("ready").exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "child exited before durable marker"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for durable marker"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let live = scan(&directory);
        assert!(live.unfinished.is_empty());
        assert!(live.errors.is_empty(), "{:?}", live.errors);
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let restarted = scan(&directory);
        assert!(restarted.errors.is_empty(), "{:?}", restarted.errors);
        assert_eq!(
            restarted.unfinished,
            vec![Record {
                label: "copy".into(),
                description: "source -> target".into(),
                started: 42,
            }]
        );
        assert_eq!(
            fs::read(foreign).unwrap(),
            b"foreign data survives process crash"
        );
    }

    #[test]
    fn dropped_worker_is_reported_without_touching_described_data() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("foreign.paw-copy");
        fs::write(&data, b"keep me").unwrap();
        let directory = root.path().join("recovery");
        drop(begin(&directory, "copy", &data.display().to_string(), 123).unwrap());
        let report = scan(&directory);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.unfinished.len(), 1);
        assert_eq!(report.unfinished[0].started, 123);
        assert_eq!(report.unfinished[0].description, data.display().to_string());
        assert_eq!(fs::read(data).unwrap(), b"keep me");
        assert_eq!(fs::read_dir(directory).unwrap().count(), 1);
    }

    #[test]
    fn live_worker_is_not_reported_as_interrupted() {
        let root = tempfile::tempdir().unwrap();
        let pending = begin(root.path(), "copy", "source -> target", 1).unwrap();
        let report = scan(root.path());
        assert!(report.unfinished.is_empty());
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        drop(pending);
        assert_eq!(scan(root.path()).unfinished.len(), 1);
    }

    #[test]
    fn explicit_completion_removes_only_owned_marker() {
        let root = tempfile::tempdir().unwrap();
        let unrelated = root.path().join("operation-foreign.json");
        fs::write(&unrelated, b"user data").unwrap();
        let pending = begin(root.path(), "copy", "source -> target", 1).unwrap();
        pending.complete().unwrap();
        assert_eq!(fs::read(unrelated).unwrap(), b"user data");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn completion_preserves_a_foreign_file_that_replaced_the_marker() {
        let root = tempfile::tempdir().unwrap();
        let pending = begin(root.path(), "copy", "source -> target", 1).unwrap();
        let original = root.path().join("original-marker");
        fs::rename(&pending.path, &original).unwrap();
        fs::write(&pending.path, b"foreign replacement").unwrap();
        let replaced_path = pending.path.clone();
        assert!(pending.complete().is_err());
        assert_eq!(fs::read(replaced_path).unwrap(), b"foreign replacement");
        assert!(original.is_file());
    }

    #[test]
    #[cfg(unix)]
    fn completion_preserves_a_symlink_that_replaced_the_marker() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let pending = begin(root.path(), "copy", "source -> target", 1).unwrap();
        fs::rename(&pending.path, root.path().join("original-marker")).unwrap();
        let foreign = root.path().join("foreign");
        fs::write(&foreign, b"keep me").unwrap();
        symlink(&foreign, &pending.path).unwrap();
        let replaced_path = pending.path.clone();
        assert!(pending.complete().is_err());
        assert!(fs::symlink_metadata(replaced_path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(foreign).unwrap(), b"keep me");
    }

    #[test]
    fn caught_panic_preserves_marker() {
        let root = tempfile::tempdir().unwrap();
        let result = std::panic::catch_unwind(|| {
            let _pending = begin(root.path(), "move", "source -> target", 1).unwrap();
            panic!("injected worker panic");
        });
        assert!(result.is_err());
        assert_eq!(scan(root.path()).unfinished.len(), 1);
    }

    #[test]
    fn corrupt_and_future_records_are_reported_and_preserved() {
        let root = tempfile::tempdir().unwrap();
        let corrupt = root.path().join("operation-corrupt.json");
        fs::write(&corrupt, b"{truncated").unwrap();
        let future = root.path().join("operation-future.json");
        let bytes = br#"{"version":999,"record":{"label":"copy","description":"x","started":1}}"#;
        fs::write(&future, bytes).unwrap();
        let report = scan(root.path());
        assert!(report.unfinished.is_empty());
        assert_eq!(report.errors.len(), 2);
        assert_eq!(fs::read(corrupt).unwrap(), b"{truncated");
        assert_eq!(fs::read(future).unwrap(), bytes);
    }

    #[test]
    fn non_markers_are_ignored_and_oversize_marker_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("notes.json"), b"unrelated").unwrap();
        let oversized = root.path().join("operation-large.json");
        fs::write(&oversized, vec![b' '; MAX_BYTES + 1]).unwrap();
        let report = scan(root.path());
        assert!(report.unfinished.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(fs::metadata(oversized).unwrap().len(), MAX_BYTES as u64 + 1);
    }

    #[test]
    fn oversized_description_and_unusable_directory_fail_before_begin() {
        let root = tempfile::tempdir().unwrap();
        assert!(begin(root.path(), "copy", &"x".repeat(MAX_BYTES + 1), 0).is_err());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        let file = root.path().join("not-a-directory");
        fs::write(&file, b"keep me").unwrap();
        assert!(begin(&file, "copy", "source -> target", 0).is_err());
        assert_eq!(fs::read(file).unwrap(), b"keep me");
    }

    #[test]
    fn absent_directory_is_empty_and_multiple_markers_are_ordered() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("recovery");
        assert!(scan(&directory).unfinished.is_empty());
        assert!(scan(&directory).errors.is_empty());
        drop(begin(&directory, "copy", "second", 2).unwrap());
        drop(begin(&directory, "move", "first", 1).unwrap());
        let report = scan(&directory);
        assert_eq!(report.unfinished.len(), 2);
        assert_eq!(report.unfinished[0].description, "first");
        assert_eq!(report.unfinished[1].description, "second");
    }

    #[test]
    #[cfg(unix)]
    fn symlink_markers_and_directories_are_never_followed() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("user-data");
        fs::write(&target, b"keep me").unwrap();
        symlink(&target, root.path().join("operation-link.json")).unwrap();
        fs::create_dir(root.path().join("operation-dir.json")).unwrap();
        let report = scan(root.path());
        assert!(report.unfinished.is_empty());
        assert_eq!(report.errors.len(), 2);
        assert_eq!(fs::read(target).unwrap(), b"keep me");
    }
}
