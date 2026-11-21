//! Conservative crash detection for operations that reached their worker.
//!
//! All I/O belongs on background workers. Legacy operation records are descriptions.
//! A separate copy receipt can be selected after its previous worker releases it.
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
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
    #[serde(default, skip_serializing)]
    pub staging: Vec<StagingRecord>,
    #[serde(default, skip_serializing)]
    pub planned_staging: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct StagingRecord {
    /// Display-only receipt. Never used as a cleanup target.
    pub location: String,
    pub identity: Option<LocalIdentity>,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum StagingEvent {
    Created(StagingRecord),
    Planned { planned: String },
    Abandoned { abandoned: String },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct LocalIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    volume: Option<u32>,
    #[cfg(windows)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_index: Option<u64>,
    #[cfg(windows)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    volume_id: Option<u64>,
    #[cfg(windows)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_id_128: Option<[u8; 16]>,
}

impl LocalIdentity {
    fn of(path: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() {
            return Err(io::Error::other("Staging path is no longer a directory"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            let identity = crate::windows_file_id::from_path(path)?;
            Ok(Self {
                volume: None,
                file_index: None,
                volume_id: identity.map(|id| id.volume),
                file_id_128: identity.map(|id| id.id),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(Self {})
        }
    }
}

thread_local! {
    static ACTIVE_MARKER: RefCell<Option<File>> = const { RefCell::new(None) };
}

/// Persist a candidate before mkdir. A crash can then report the location,
/// without claiming the directory was created or belongs to this operation.
pub(crate) fn plan_staging(location: &crate::vfs::Location) -> io::Result<()> {
    append_staging_event(&StagingEvent::Planned {
        planned: location.display(),
    })
}

pub(crate) fn abandon_staging(location: &crate::vfs::Location) -> io::Result<()> {
    append_staging_event(&StagingEvent::Abandoned {
        abandoned: location.display(),
    })
}

/// Confirm a directory created by the current worker before writing payload.
pub(crate) fn register_staging(location: &crate::vfs::Location) -> io::Result<()> {
    let receipt = StagingRecord {
        location: location.display(),
        identity: location.local().map(LocalIdentity::of).transpose()?,
    };
    append_staging_event(&StagingEvent::Created(receipt))
}

/// A partial final line after a crash is reported while complete earlier
/// receipts remain readable. The existing worker marker supplies provenance.
fn append_staging_event(event: &StagingEvent) -> io::Result<()> {
    ACTIVE_MARKER.with(|active| {
        let mut active = active.borrow_mut();
        let Some(file) = active.as_mut() else {
            return Ok(());
        };
        let mut bytes = serde_json::to_vec(event).map_err(io::Error::other)?;
        bytes.push(b'\n');
        let length = file.metadata()?.len();
        if length.saturating_add(bytes.len() as u64) > MAX_BYTES as u64 {
            return Err(io::Error::other("Recovery staging journal exceeds 128 KiB"));
        }
        file.seek(SeekFrom::End(0))?;
        file.write_all(&bytes)?;
        file.sync_all()
    })
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
    #[cfg(target_os = "macos")]
    copies: Vec<crate::vfs::resume_copy::Offer>,
}

impl Report {
    fn has_findings(&self) -> bool {
        #[cfg(target_os = "macos")]
        if !self.copies.is_empty() {
            return true;
        }
        !self.unfinished.is_empty() || !self.errors.is_empty()
    }
}

/// Startup discovery and presentation; disk access stays on its scan worker.
#[derive(Default)]
pub(crate) struct Recovery {
    report: Report,
    receive: Option<Receiver<Report>>,
    directory: Option<PathBuf>,
    open: bool,
    #[cfg(target_os = "macos")]
    resume_requested: Option<crate::vfs::resume_copy::Offer>,
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
                    ..Default::default()
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
        let mut recovery = Self {
            directory: Some(directory),
            ..Self::default()
        };
        recovery.scan_in_background(ctx);
        recovery
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
                self.open |= report.has_findings();
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

    pub fn reopen(&mut self, ctx: &egui::Context) {
        self.open = true;
        if cfg!(target_os = "macos") {
            self.scan_in_background(ctx);
        }
    }

    fn scan_in_background(&mut self, ctx: &egui::Context) {
        let Some(directory) = self.directory.clone() else {
            return;
        };
        if self.receive.is_some() {
            return;
        }
        let (send, receive) = mpsc::channel();
        let ctx = ctx.clone();
        match std::thread::Builder::new()
            .name("pawcommander-recovery-scan".into())
            .spawn(move || {
                let _ = send.send(scan(&directory));
                ctx.request_repaint();
            }) {
            Ok(_) => self.receive = Some(receive),
            Err(error) => {
                self.report.errors.push(error.to_string());
                self.open = true;
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub fn take_resume_request(&mut self) -> Option<crate::vfs::resume_copy::Offer> {
        self.resume_requested.take()
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        #[cfg(target_os = "macos")]
        let mut resume = None;
        egui::Window::new(crate::tr_format!("Восстановление после сбоя", "Crash recovery"))
            .id(egui::Id::unique("crash_recovery"))
            .open(&mut self.open)
            .default_size([780.0, 440.0])
            .show(ctx, |ui| {
                ui.label(crate::tr_format!(
                    "Обнаружены задания без подтверждённого завершения. Некоторые файлы могли быть обработаны. Сверьте обе папки и журнал результатов перед повтором.",
                    "These operations have no confirmed completion. Some files may have been processed. Check both folders and result history before retrying."
                ));
                ui.label(crate::tr_format!(
                    "Временные копии перечислены только по сохранённым записям; их текущее содержимое не проверено. Запланированный адрес может содержать чужие данные. Ожидающие задания не отслеживаются. Автоматический повтор и удаление файлов не выполняются.",
                    "Temporary copies are listed only from saved receipts; their current contents are unverified. A planned location may contain someone else's data. Queued jobs are not tracked. No automatic retry or file removal is performed."
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
                } else if !self.report.has_findings() {
                    ui.label(crate::tr_format!("Незавершённые задания не обнаружены.", "No unfinished operations found."));
                }
                egui::ScrollArea::both().show(ui, |ui| {
                    #[cfg(target_os = "macos")]
                    if !self.report.copies.is_empty() {
                        ui.heading(crate::i18n::text!("Прерванные копирования и переносы"));
                        ui.small(crate::i18n::text!("Перед продолжением PawCommander сверяет источник, назначение и временную копию."));
                        for (index, offer) in self.report.copies.iter().enumerate() {
                            ui.push_id(&offer.id, |ui| {
                                ui.label(format!("{} → {}", offer.source, offer.target));
                                let action = if offer.moving { crate::i18n::text!("Проверить и продолжить перенос") } else { crate::i18n::text!("Проверить и продолжить") };
                                if offer.moving {
                                    ui.small(crate::i18n::text!("Источник удаляется после проверки полной копии."));
                                }
                                if ui.add_enabled(self.receive.is_none(), egui::Button::new(action)).clicked() {
                                    resume = Some(index);
                                }
                            });
                        }
                        ui.separator();
                    }
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
                        for staging in &record.staging {
                            ui.add(egui::Label::new(crate::tr_format!(
                                "Зарегистрирована временная копия: {}",
                                "Registered temporary copy: {}",
                                staging.location
                            )).selectable(true).wrap());
                        }
                        for planned in &record.planned_staging {
                            ui.add(egui::Label::new(crate::tr_format!(
                                "Запланированная временная папка (создание не подтверждено): {}",
                                "Planned temporary folder (creation unconfirmed): {}",
                                planned
                            )).selectable(true).wrap());
                        }
                    }
                });
            });
        #[cfg(target_os = "macos")]
        if let Some(index) = resume {
            self.resume_requested = Some(self.report.copies.remove(index));
            self.open = false;
        }
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
            staging: Vec::new(),
            planned_staging: Vec::new(),
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
    temporary.write_all(b"\n")?;
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
    /// Activate this marker only on the worker thread running its operation.
    pub fn run<R>(&self, work: impl FnOnce() -> R) -> io::Result<R> {
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                ACTIVE_MARKER.with(|active| *active.borrow_mut() = None);
            }
        }
        let clone = self.file.try_clone()?;
        ACTIVE_MARKER.with(|active| {
            let mut active = active.borrow_mut();
            if active.is_some() {
                return Err(io::Error::other(
                    "Recovery worker already has an active marker",
                ));
            }
            *active = Some(clone);
            Ok(())
        })?;
        let _clear = Clear;
        Ok(work())
    }

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

pub(crate) fn same_owned_marker(owned: &File, path: &Path) -> io::Result<bool> {
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
            Ok(Some((record, warnings))) => {
                for warning in warnings {
                    report.errors.push(format!("{}: {warning}", path.display()));
                }
                report_bytes = report_bytes
                    .saturating_add(record.label.len())
                    .saturating_add(record.description.len())
                    .saturating_add(
                        record
                            .staging
                            .iter()
                            .map(|entry| entry.location.len())
                            .sum::<usize>(),
                    )
                    .saturating_add(
                        record
                            .planned_staging
                            .iter()
                            .map(String::len)
                            .sum::<usize>(),
                    );
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
    #[cfg(target_os = "macos")]
    {
        let (copies, errors) = crate::vfs::resume_copy::scan(directory);
        report.copies = copies;
        report.errors.extend(errors);
    }
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

fn read_record(path: &Path) -> io::Result<Option<(Record, Vec<String>)>> {
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
    let first_end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(bytes.len());
    let saved: Saved = serde_json::from_slice(&bytes[..first_end]).map_err(io::Error::other)?;
    if saved.version != VERSION {
        return Err(io::Error::other("Unsupported recovery record version"));
    }
    let mut record = saved.record;
    let mut warnings = Vec::new();
    if first_end < bytes.len() {
        let mut remainder = &bytes[first_end + 1..];
        while !remainder.is_empty() {
            let Some(end) = remainder.iter().position(|byte| *byte == b'\n') else {
                warnings.push("Incomplete staging receipt preserved".into());
                break;
            };
            if end > 0 {
                match serde_json::from_slice::<StagingEvent>(&remainder[..end]) {
                    Ok(StagingEvent::Created(receipt)) => {
                        record
                            .planned_staging
                            .retain(|path| *path != receipt.location);
                        record.staging.push(receipt);
                    }
                    Ok(StagingEvent::Planned { planned }) => record.planned_staging.push(planned),
                    Ok(StagingEvent::Abandoned { abandoned }) => {
                        record.planned_staging.retain(|path| *path != abandoned);
                    }
                    Err(error) => {
                        warnings.push(format!("Invalid staging receipt preserved: {error}"))
                    }
                }
            }
            remainder = &remainder[end + 1..];
        }
    }
    Ok(Some((record, warnings)))
}

pub(crate) fn sync_directory(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn legacy_windows_staging_identity_remains_readable() {
        let old = r#"{"volume":42,"file_index":7}"#;
        let identity: LocalIdentity = serde_json::from_str(old).unwrap();
        assert_eq!(identity.volume, Some(42));
        assert_eq!(identity.file_index, Some(7));
        assert_eq!(identity.volume_id, None);
        assert_eq!(identity.file_id_128, None);
    }

    #[test]
    #[ignore = "subprocess entry point for killed_worker_is_recovered"]
    fn crash_process_helper() {
        let directory =
            PathBuf::from(std::env::var_os("PAWCOMMANDER_RECOVERY_CRASH_TEST_DIR").unwrap());
        let staging = directory.parent().unwrap().join(".pawcommander-copy-owned");
        fs::create_dir(&staging).unwrap();
        let pending = begin(&directory, "copy", "source -> target", 42).unwrap();
        pending
            .run(|| {
                register_staging(&crate::vfs::Location::Local(staging.clone())).unwrap();
                fs::write(staging.join("payload"), b"incomplete copy").unwrap();
            })
            .unwrap();
        fs::write(directory.parent().unwrap().join("ready"), b"ready").unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
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
                staging: vec![StagingRecord {
                    location: root
                        .path()
                        .join(".pawcommander-copy-owned")
                        .display()
                        .to_string(),
                    identity: Some(
                        LocalIdentity::of(&root.path().join(".pawcommander-copy-owned")).unwrap()
                    ),
                }],
                planned_staging: Vec::new(),
            }]
        );
        assert_eq!(
            fs::read(root.path().join(".pawcommander-copy-owned/payload")).unwrap(),
            b"incomplete copy"
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
    fn only_active_owned_marker_receives_staging_receipts() {
        let root = tempfile::tempdir().unwrap();
        let recovery = root.path().join("recovery");
        let owned = root.path().join(".pawcommander-copy-owned");
        let foreign = root.path().join(".pawcommander-copy-foreign");
        fs::create_dir(&owned).unwrap();
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("payload"), b"foreign").unwrap();
        let pending = begin(&recovery, "copy", "one item", 5).unwrap();
        pending
            .run(|| {
                register_staging(&crate::vfs::Location::Local(owned.clone())).unwrap();
                assert!(scan(&recovery).unfinished.is_empty());
            })
            .unwrap();
        drop(pending);
        let report = scan(&recovery);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.unfinished.len(), 1);
        assert_eq!(report.unfinished[0].staging.len(), 1);
        assert!(report.unfinished[0].planned_staging.is_empty());
        assert_eq!(
            report.unfinished[0].staging[0].location,
            owned.display().to_string()
        );
        assert_eq!(fs::read(foreign.join("payload")).unwrap(), b"foreign");
        assert!(owned.is_dir());
    }

    #[test]
    fn crash_after_mkdir_before_confirmation_reports_an_unconfirmed_candidate() {
        let root = tempfile::tempdir().unwrap();
        let recovery = root.path().join("recovery");
        let candidate = root.path().join(".pawcommander-copy-planned");
        let foreign = root.path().join(".pawcommander-copy-foreign");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("payload"), b"foreign").unwrap();
        let pending = begin(&recovery, "copy", "one item", 5).unwrap();
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pending
                .run(|| {
                    plan_staging(&crate::vfs::Location::Local(candidate.clone())).unwrap();
                    fs::create_dir(&candidate).unwrap();
                    panic!("injected crash after mkdir");
                })
                .unwrap();
        }));
        assert!(stopped.is_err());
        drop(pending);
        let report = scan(&recovery);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.unfinished.len(), 1);
        assert!(report.unfinished[0].staging.is_empty());
        assert_eq!(
            report.unfinished[0].planned_staging,
            vec![candidate.display().to_string()]
        );
        assert!(candidate.is_dir());
        assert_eq!(fs::read(foreign.join("payload")).unwrap(), b"foreign");
    }

    #[test]
    fn rejected_candidate_is_not_reported_as_an_unfinished_copy() {
        let root = tempfile::tempdir().unwrap();
        let foreign = root.path().join(".pawcommander-copy-foreign");
        fs::create_dir(&foreign).unwrap();
        let pending = begin(root.path(), "copy", "one item", 5).unwrap();
        pending
            .run(|| {
                let location = crate::vfs::Location::Local(foreign.clone());
                plan_staging(&location).unwrap();
                abandon_staging(&location).unwrap();
            })
            .unwrap();
        drop(pending);
        let report = scan(root.path());
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.unfinished[0].planned_staging.is_empty());
        assert!(foreign.is_dir());
    }

    #[test]
    fn incomplete_staging_receipt_preserves_prior_receipts() {
        let root = tempfile::tempdir().unwrap();
        let owned = root.path().join("owned");
        fs::create_dir(&owned).unwrap();
        let pending = begin(root.path(), "copy", "one item", 5).unwrap();
        pending
            .run(|| register_staging(&crate::vfs::Location::Local(owned.clone())).unwrap())
            .unwrap();
        let path = pending.path.clone();
        drop(pending);
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"{partial")
            .unwrap();
        let report = scan(root.path());
        assert_eq!(report.unfinished.len(), 1);
        assert_eq!(report.unfinished[0].staging.len(), 1);
        assert_eq!(report.errors.len(), 1);
    }

    #[test]
    fn interrupted_local_copy_is_listed_with_its_payload() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let target = root.path().join("target.txt");
        fs::write(&source, b"source data").unwrap();
        let recovery = root.path().join("recovery");
        let pending = begin(&recovery, "copy", "source -> target", 6).unwrap();
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pending
                .run(|| {
                    crate::fs_ops::copy_atomically_with_progress(
                        &source,
                        &target,
                        &mut crate::operations::Progress::default(),
                        |_, _| panic!("injected crash before publication"),
                    )
                })
                .unwrap()
                .unwrap();
        }));
        assert!(stopped.is_err());
        drop(pending);
        let report = scan(&recovery);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.unfinished.len(), 1);
        let registered = &report.unfinished[0].staging;
        assert_eq!(registered.len(), 1);
        assert!(report.unfinished[0].planned_staging.is_empty());
        assert!(registered[0].identity.is_some());
        let staging = Path::new(&registered[0].location);
        assert_eq!(fs::read(staging.join("payload")).unwrap(), b"source data");
        assert!(!target.exists());
        assert_eq!(fs::read(source).unwrap(), b"source data");
    }

    #[test]
    fn live_worker_is_not_reported_as_interrupted() {
        let root = tempfile::tempdir().unwrap();
        let pending = begin(root.path(), "copy", "source -> target", 1).unwrap();
        let report = scan(root.path());
        assert!(report.unfinished.is_empty());
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        drop(pending);
        let stopped = scan(root.path());
        assert_eq!(stopped.unfinished.len(), 1, "{:?}", stopped.errors);
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
