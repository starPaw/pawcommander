//! Completed operation results. Disk access and serialization belong to workers.
use crate::vfs::Location;
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{SystemTime, UNIX_EPOCH};

const VERSION: u32 = 1;
const KEEP_OPERATIONS: usize = 100;
const KEEP_ROWS: usize = 100_000;
const MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Outcome {
    Completed,
    CompletedWithWarning,
    Skipped,
    Failed,
    Cancelled,
    #[default]
    NotStarted,
}

impl Outcome {
    fn label(self) -> &'static str {
        crate::i18n::text(match self {
            Self::Completed => "Завершено",
            Self::CompletedWithWarning => "Завершено с предупреждением",
            Self::Skipped => "Пропущено",
            Self::Failed => "Ошибка",
            Self::Cancelled => "Отменено",
            Self::NotStarted => "Не начато",
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ItemResult {
    pub source: String,
    pub target: Option<String>,
    pub outcome: Outcome,
    pub reason: Option<String>,
}

impl ItemResult {
    pub fn new(source: String, target: Option<String>) -> Self {
        Self {
            source,
            target,
            outcome: Outcome::NotStarted,
            reason: None,
        }
    }
}

/// The active item is explicit: progress paths also include probes and staging.
#[derive(Default)]
pub(crate) struct Results {
    pub items: Vec<ItemResult>,
    active: Option<usize>,
    index: HashMap<Location, usize>,
    pub cancelled: bool,
}

impl Results {
    pub fn plan(&mut self, items: Vec<(Location, Option<String>)>) {
        self.items.clear();
        self.index.clear();
        for (source, target) in items {
            self.index.insert(source.clone(), self.items.len());
            self.items.push(ItemResult::new(source.display(), target));
        }
        self.active = None;
    }

    pub fn begin(&mut self, source: &Location) {
        self.active = self.index.get(source).copied();
    }

    pub fn clear_active(&mut self) {
        self.active = None;
    }

    pub fn target(&mut self, source: &Location, target: String) {
        if let Some(&index) = self.index.get(source) {
            self.items[index].target = Some(target);
        }
    }

    pub fn record(
        &mut self,
        source: &Location,
        target: Option<String>,
        outcome: Outcome,
        reason: Option<String>,
    ) {
        if let Some(row) = self.index.get(source).map(|&index| &mut self.items[index]) {
            row.target = target.or_else(|| row.target.take());
            row.outcome = outcome;
            row.reason = reason;
        }
        self.active = None;
    }

    pub fn finish(
        mut self,
        label: String,
        started: u64,
        result: &Result<String, String>,
    ) -> Option<Record> {
        if self.items.is_empty() {
            return None;
        }
        let reason = result.as_ref().err().cloned().or_else(|| {
            self.cancelled
                .then(|| crate::i18n::text("Операция остановлена пользователем").into())
        });
        if let Some(index) = self.active {
            let row = &mut self.items[index];
            row.outcome = if result.is_err() {
                Outcome::Failed
            } else if self.cancelled {
                Outcome::Cancelled
            } else {
                Outcome::Completed
            };
            row.reason.clone_from(&reason);
        }
        for row in &mut self.items {
            if row.outcome == Outcome::NotStarted {
                row.reason = if result.is_err() {
                    Some(
                        crate::i18n::text(
                            "Не начато после ошибки; причина указана в итоге операции",
                        )
                        .into(),
                    )
                } else if self.cancelled {
                    reason.clone()
                } else {
                    None
                };
            }
        }
        Some(Record {
            label,
            started,
            finished: now(),
            failed: result.is_err(),
            cancelled: self.cancelled,
            message: result.as_ref().unwrap_or_else(|error| error).clone(),
            items: self.items,
        })
    }
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Record {
    label: String,
    started: u64,
    finished: u64,
    failed: bool,
    cancelled: bool,
    message: String,
    pub items: Vec<ItemResult>,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    version: u32,
    records: Vec<Record>,
}

impl Saved {
    fn push(&mut self, record: Record) {
        self.records.push(record);
        let mut rows = 0;
        let mut retain = self.records.len();
        for record in self.records.iter().rev().take(KEEP_OPERATIONS) {
            if retain < self.records.len() && rows + record.items.len() > KEEP_ROWS {
                break;
            }
            rows += record.items.len();
            retain -= 1;
        }
        self.records.drain(..retain);
    }
}

struct Update {
    records: Vec<Record>,
    error: Option<String>,
}

struct Append {
    record: Record,
    saved: Sender<()>,
}

#[derive(Clone, Default)]
pub(crate) struct HistorySender(Option<Sender<Append>>);

impl HistorySender {
    /// The operation worker waits for persistence before publishing completion.
    pub fn append(&self, record: Record) {
        if let Some(sender) = &self.0 {
            let (saved, wait) = mpsc::channel();
            if sender.send(Append { record, saved }).is_ok() {
                let _ = wait.recv();
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct History {
    sender: HistorySender,
    updates: Option<Receiver<Update>>,
    records: Vec<Record>,
    pub error: Option<String>,
    path: Option<PathBuf>,
    loaded: bool,
}

impl History {
    pub fn new(ctx: &egui::Context) -> Self {
        // Unit-test app instances must never touch the user's real history.
        if cfg!(test) {
            return Self::default();
        }
        match dirs::data_local_dir() {
            Some(root) => Self::open(
                root.join("PawCommander").join("operation-history.json"),
                ctx,
            ),
            None => Self {
                error: Some("No local application-data directory".into()),
                ..Self::default()
            },
        }
    }

    fn open(path: PathBuf, ctx: &egui::Context) -> Self {
        let (send, requests) = mpsc::channel::<Append>();
        let (updates, receive) = mpsc::channel();
        let worker_path = path.clone();
        let ctx = ctx.clone();
        let worker = std::thread::Builder::new()
            .name("pawcommander-result-history".into())
            .spawn(move || {
                let opened = open_store(&worker_path);
                let (mut saved, lock, mut error) = match opened {
                    Ok((saved, lock)) => (saved, Some(lock), None),
                    Err(error) => (
                        Saved {
                            version: VERSION,
                            ..Saved::default()
                        },
                        None,
                        Some(error.to_string()),
                    ),
                };
                let _ = updates.send(Update {
                    records: saved.records.clone(),
                    error: error.clone(),
                });
                ctx.request_repaint();
                for request in requests {
                    saved.push(request.record);
                    if lock.is_some() {
                        error = save(&worker_path, &saved)
                            .err()
                            .map(|error| error.to_string());
                    }
                    let _ = updates.send(Update {
                        records: saved.records.clone(),
                        error: error.clone(),
                    });
                    ctx.request_repaint();
                    let _ = request.saved.send(());
                }
            });
        match worker {
            Ok(_) => Self {
                sender: HistorySender(Some(send)),
                updates: Some(receive),
                path: Some(path),
                ..Self::default()
            },
            Err(error) => Self {
                error: Some(error.to_string()),
                path: Some(path),
                ..Self::default()
            },
        }
    }

    pub fn sender(&self) -> HistorySender {
        self.sender.clone()
    }

    pub fn poll(&mut self) {
        if let Some(updates) = &self.updates {
            loop {
                match updates.try_recv() {
                    Ok(update) => {
                        self.records = update.records;
                        self.error = update.error;
                        self.loaded = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.error = Some("History worker disconnected".into());
                        break;
                    }
                }
            }
        }
    }

    pub fn show(&self, ctx: &egui::Context, open: &mut bool) {
        egui::Window::new(crate::i18n::text("Журнал результатов"))
            .id(egui::Id::new("result_history"))
            .open(open)
            .default_size([780.0, 560.0])
            .show(ctx, |ui| {
                ui.label(crate::i18n::text(
                    "Результаты выбранных объектов; папки показаны целиком.",
                ));
                ui.label(crate::i18n::text(
                    "Последние 100 операций; старые записи удаляются при достижении 100 000 строк.",
                ));
                if let Some(path) = &self.path {
                    ui.add(egui::Label::new(path.display().to_string()).selectable(true));
                }
                if let Some(error) = &self.error {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        crate::i18n::text(
                            "История не сохранена. Проверьте файл журнала и доступ к нему.",
                        ),
                    );
                    ui.add(egui::Label::new(error).selectable(true));
                }
                if self.records.is_empty() {
                    ui.label(crate::i18n::text(if self.loaded {
                        "Журнал пуст"
                    } else {
                        "Загрузка журнала…"
                    }));
                }
                egui::ScrollArea::vertical()
                    .id_salt("result_operations")
                    .show(ui, |ui| {
                        for (index, record) in self.records.iter().enumerate().rev() {
                            let time = chrono::DateTime::from_timestamp(record.started as i64, 0)
                                .map(|time| {
                                    time.with_timezone(&chrono::Local)
                                        .format("%Y-%m-%d %H:%M:%S")
                                        .to_string()
                                })
                                .unwrap_or_default();
                            egui::CollapsingHeader::new(format!(
                                "{time} · {} · {}",
                                record.label,
                                record.items.len()
                            ))
                            .id_salt((index, record.started, record.finished))
                            .default_open(index + 1 == self.records.len())
                            .show(ui, |ui| {
                                ui.add(egui::Label::new(&record.message).selectable(true).wrap());
                                egui::ScrollArea::both()
                                    .id_salt(("result_items", index))
                                    .max_height(360.0)
                                    .show_rows(
                                        ui,
                                        ui.text_style_height(&egui::TextStyle::Body) * 3.0,
                                        record.items.len(),
                                        |ui, range| {
                                            for row in &record.items[range] {
                                                let target = row.target.as_deref().unwrap_or("—");
                                                ui.add(
                                                    egui::Label::new(format!(
                                                        "{}\n{} → {}\n{}",
                                                        row.outcome.label(),
                                                        single_line(&row.source),
                                                        single_line(target),
                                                        single_line(
                                                            row.reason.as_deref().unwrap_or("—")
                                                        )
                                                    ))
                                                    .selectable(true)
                                                    .extend(),
                                                );
                                            }
                                        },
                                    );
                            });
                        }
                    });
            });
    }
}

fn single_line(text: &str) -> String {
    text.replace('\r', "\\r").replace('\n', "\\n")
}

fn open_store(path: &Path) -> io::Result<(Saved, File)> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Missing history parent"))?;
    fs::create_dir_all(parent)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))?;
    lock.try_lock().map_err(io::Error::other)?;
    let saved = match File::open(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err(io::Error::other(
                    "History exceeds 32 MiB; existing file preserved",
                ));
            }
            let saved: Saved = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            if saved.version != VERSION {
                return Err(io::Error::other(
                    "Unsupported history version; existing file preserved",
                ));
            }
            saved
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Saved {
            version: VERSION,
            ..Saved::default()
        },
        Err(error) => return Err(error),
    };
    Ok((saved, lock))
}

fn save(path: &Path, saved: &Saved) -> io::Result<()> {
    let bytes = serde_json::to_vec(saved).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::other(
            "History exceeds 32 MiB; existing file preserved",
        ));
    }
    let mut file = tempfile::NamedTempFile::new_in(
        path.parent()
            .ok_or_else(|| io::Error::other("Missing history parent"))?,
    )?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Record {
        let mut results = Results::default();
        results.plan(vec![(local("source"), Some("target".into()))]);
        results.begin(&local("source"));
        results
            .finish("copy".into(), 1, &Ok("Copied".into()))
            .unwrap()
    }

    fn local(path: &str) -> Location {
        Location::Local(PathBuf::from(path))
    }

    #[test]
    #[cfg(unix)]
    fn non_utf8_display_collisions_never_merge_result_identity() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let first = Location::Local(PathBuf::from(OsString::from_vec(b"same\xff".to_vec())));
        let second = Location::Local(PathBuf::from(OsString::from_vec(b"same\xfe".to_vec())));
        assert_eq!(first.display(), second.display());
        let mut results = Results::default();
        results.plan(vec![(first.clone(), None), (second.clone(), None)]);
        results.record(&first, None, Outcome::Skipped, Some("conflict".into()));
        results.begin(&second);
        let record = results
            .finish("copy".into(), 0, &Ok("copied".into()))
            .unwrap();
        assert_eq!(record.items[0].outcome, Outcome::Skipped);
        assert_eq!(record.items[0].reason.as_deref(), Some("conflict"));
        assert_eq!(record.items[1].outcome, Outcome::Completed);
    }

    #[test]
    fn history_virtual_rows_keep_multiline_reasons_on_three_display_lines() {
        let ctx = egui::Context::default();
        let mut entry = record();
        entry.items = (0..100)
            .map(|index| ItemResult {
                source: format!("source-{index:03}"),
                target: Some("target\nwith newline".into()),
                outcome: Outcome::Failed,
                reason: Some("first line\nsecond line\r\nthird line".into()),
            })
            .collect();
        let history = History {
            records: vec![entry],
            loaded: true,
            ..History::default()
        };
        let mut open = true;
        let mut draw = |events| {
            ctx.run(
                egui::RawInput {
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(960.0, 700.0),
                    )),
                    ..Default::default()
                },
                |ctx| history.show(ctx, &mut open),
            )
        };
        draw(vec![]);
        let output = draw(vec![]);
        let mut item_bounds = Vec::new();
        for clipped in &output.shapes {
            if let egui::Shape::Text(text) = &clipped.shape {
                if text.galley.text().contains("source-") {
                    assert_eq!(text.galley.rows.len(), 3);
                    assert!(text.galley.text().contains("second line\\r\\nthird line"));
                    item_bounds.push(egui::Rect::from_min_size(text.pos, text.galley.size()));
                }
            }
        }
        assert!(item_bounds.len() >= 3);
        for pair in item_bounds.windows(2) {
            assert!(
                pair[0].bottom() <= pair[1].top(),
                "virtual result rows overlap"
            );
        }
        let pointer = item_bounds[1].center();
        let mut output = output;
        for _ in 0..6 {
            output = draw(vec![
                egui::Event::PointerMoved(pointer),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -10_000.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
        }
        let last = output
            .shapes
            .iter()
            .find_map(|clipped| {
                if let egui::Shape::Text(text) = &clipped.shape {
                    if text.galley.text().contains("source-099") {
                        return Some((clipped.clip_rect, text));
                    }
                }
                None
            })
            .expect("last result remains reachable by scrolling");
        assert!(
            last.0
                .contains_rect(egui::Rect::from_min_size(last.1.pos, last.1.galley.size())),
            "last row is fully visible"
        );
    }

    #[test]
    fn save_round_trip_reopens_all_outcomes_and_recovery_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let (mut saved, lock) = open_store(&path).unwrap();
        let mut item = record();
        for outcome in [
            Outcome::CompletedWithWarning,
            Outcome::Skipped,
            Outcome::Failed,
            Outcome::Cancelled,
            Outcome::NotStarted,
        ] {
            item.items.push(ItemResult {
                source: "sftp://user@host:22/кириллица".into(),
                target: None,
                outcome,
                reason: Some(
                    "Check original target and staging /private/recovery\nsecond line".into(),
                ),
            });
        }
        saved.push(item);
        save(&path, &saved).unwrap();
        drop(lock);
        let (reopened, _lock) = open_store(&path).unwrap();
        assert_eq!(reopened.records[0].items.len(), 6);
        assert_eq!(
            reopened.records[0].items[1].outcome,
            Outcome::CompletedWithWarning
        );
        assert!(reopened.records[0].items[1]
            .reason
            .as_ref()
            .unwrap()
            .contains("\nsecond line"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            2,
            "only history and lock; no staging files"
        );
    }

    #[test]
    fn worker_acknowledges_only_after_atomic_save_and_preserves_corrupt_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let ctx = egui::Context::default();
        let mut history = History::open(path.clone(), &ctx);
        history.sender().append(record());
        let saved: Saved = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved.records.len(),
            1,
            "completion acknowledgment means persistence attempted"
        );
        history.poll();
        assert!(history.error.is_none());
        assert_eq!(history.records.len(), 1);

        let corrupt = dir.path().join("corrupt.json");
        fs::write(&corrupt, b"{incomplete history").unwrap();
        let mut history = History::open(corrupt.clone(), &ctx);
        history.sender().append(record());
        history.poll();
        assert!(history.error.is_some());
        assert_eq!(
            history.records.len(),
            1,
            "current results remain reviewable in memory"
        );
        assert_eq!(fs::read(corrupt).unwrap(), b"{incomplete history");
    }

    #[test]
    fn concurrent_instances_and_unknown_versions_never_overwrite_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let (saved, lock) = open_store(&path).unwrap();
        save(&path, &saved).unwrap();
        let baseline = fs::read(&path).unwrap();
        assert!(open_store(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), baseline);
        drop(lock);
        fs::write(&path, br#"{"version":999,"records":[]}"#).unwrap();
        assert!(open_store(&path).is_err());
        assert_eq!(fs::read(path).unwrap(), br#"{"version":999,"records":[]}"#);
    }

    #[test]
    fn oversized_snapshot_preserves_previous_file_and_removes_tempfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let (mut saved, _lock) = open_store(&path).unwrap();
        saved.push(record());
        save(&path, &saved).unwrap();
        let baseline = fs::read(&path).unwrap();
        saved.records[0].message = "x".repeat(MAX_BYTES as usize);
        assert!(save(&path, &saved).is_err());
        assert_eq!(fs::read(&path).unwrap(), baseline);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn retention_keeps_newest_operation_whole_and_bounds_older_records() {
        let mut saved = Saved {
            version: VERSION,
            records: Vec::new(),
        };
        for index in 0..105 {
            let mut row = record();
            row.started = index;
            saved.push(row);
        }
        assert_eq!(saved.records.len(), KEEP_OPERATIONS);
        assert_eq!(saved.records[0].started, 5);
        let mut large = record();
        large.items = vec![large.items[0].clone(); KEEP_ROWS + 1];
        saved.push(large);
        assert_eq!(saved.records.len(), 1);
        assert_eq!(saved.records[0].items.len(), KEEP_ROWS + 1);
        saved.push(record());
        assert_eq!(saved.records.len(), 1);
    }

    #[test]
    fn terminal_failure_does_not_reclassify_published_or_skipped_items() {
        let mut results = Results::default();
        results.plan(
            ["published", "skipped", "failed", "pending"]
                .map(|source| (local(source), None))
                .into(),
        );
        results.record(
            &local("published"),
            None,
            Outcome::CompletedWithWarning,
            Some("cleanup denied; inspect staging".into()),
        );
        results.record(
            &local("skipped"),
            None,
            Outcome::Skipped,
            Some("conflict".into()),
        );
        results.begin(&local("failed"));
        let record = results
            .finish("copy".into(), 0, &Err("source disappeared".into()))
            .unwrap();
        assert_eq!(
            record
                .items
                .iter()
                .map(|row| row.outcome)
                .collect::<Vec<_>>(),
            [
                Outcome::CompletedWithWarning,
                Outcome::Skipped,
                Outcome::Failed,
                Outcome::NotStarted
            ]
        );
        assert_eq!(
            record.items[0].reason.as_deref(),
            Some("cleanup denied; inspect staging")
        );
        assert_eq!(
            record.items[3].reason.as_deref(),
            Some(crate::i18n::text(
                "Не начато после ошибки; причина указана в итоге операции"
            ))
        );
    }
}
