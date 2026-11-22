//! Completed operation results. Disk access and serialization belong to workers.
use crate::file_error::MetadataWarning;
use crate::operations::TransferError;
use crate::recovery::{same_owned_marker, sync_directory};
#[cfg(test)]
use crate::test_ui::ContextExt as _;
use crate::vfs::Location;
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const VERSION: u32 = 1;
const KEEP_OPERATIONS: usize = 100;
const KEEP_ROWS: usize = 100_000;
const MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RECORD_WEIGHT: usize = MAX_BYTES as usize / 2;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const ROW_WEIGHT: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Outcome {
    Completed,
    CompletedWithWarning,
    CopiedSourceRemaining,
    Skipped,
    Failed,
    Cancelled,
    PublicationUnconfirmed,
    PartialCopyRetained,
    RetryReady,
    #[default]
    NotStarted,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Self::Completed => crate::i18n::text!("Завершено"),
            Self::CompletedWithWarning => crate::i18n::text!("Завершено с предупреждением"),
            Self::CopiedSourceRemaining => crate::i18n::text!("Копия создана, перенос не завершён"),
            Self::Skipped => crate::i18n::text!("Пропущено"),
            Self::Failed => crate::i18n::text!("Ошибка"),
            Self::Cancelled => crate::i18n::text!("Отменено"),
            Self::PublicationUnconfirmed => crate::i18n::text!("Публикация не подтверждена"),
            Self::PartialCopyRetained => {
                crate::i18n::text!("Доступна проверка прерванного копирования")
            }
            Self::RetryReady => crate::i18n::text!("Готово к повтору"),
            Self::NotStarted => crate::i18n::text!("Не начато"),
        }
    }
}

pub(crate) enum ChecksumOutcome {
    Matched,
    Changed,
    Missing,
    Unreadable(TransferError),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ItemResult {
    pub source: String,
    pub target: Option<String>,
    pub outcome: Outcome,
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_warning: Option<String>,
    #[serde(default)]
    pub parent: Option<usize>,
    #[serde(default)]
    pub descendants_enumerated: bool,
}

impl ItemResult {
    pub fn new(source: String, target: Option<String>) -> Self {
        Self {
            source,
            target,
            outcome: Outcome::NotStarted,
            reason: None,
            metadata_warning: None,
            parent: None,
            descendants_enumerated: false,
        }
    }
}

/// The active item is explicit: progress paths also include probes and staging.
#[derive(Default)]
pub(crate) struct Results {
    pub id: String,
    pub items: Vec<ItemResult>,
    active: Option<usize>,
    index: HashMap<Location, usize>,
    active_source: Option<Location>,
    nested_index: HashMap<Location, usize>,
    group: Option<(usize, std::ops::Range<usize>)>,
    observed_group: bool,
    confirming_move: bool,
    unrecorded_metadata_warning: bool,
    selected_items_only: bool,
    weight: usize,
    omitted: usize,
    details_truncated: bool,
    pub cancelled: bool,
}

impl Results {
    pub(crate) fn root_outcomes(&self) -> Vec<(Location, Outcome)> {
        self.index
            .iter()
            .map(|(path, &index)| (path.clone(), self.items[index].outcome))
            .collect()
    }
    pub fn plan(&mut self, items: impl IntoIterator<Item = (Location, Option<String>)>) {
        self.items.clear();
        self.index.clear();
        self.nested_index.clear();
        self.group = None;
        self.observed_group = false;
        self.confirming_move = false;
        self.selected_items_only = false;
        self.cancelled = false;
        self.weight = 0;
        self.omitted = 0;
        self.details_truncated = false;
        for (source, target) in items {
            if let Some(index) = self.add_row(&source, target, None) {
                self.index.insert(source, index);
            }
        }
        self.clear_active();
    }

    /// Record an operation defined by an explicit list of paths.
    pub(crate) fn plan_selected_items(&mut self, items: impl IntoIterator<Item = Location>) {
        self.plan(items.into_iter().map(|source| (source, None)));
        self.selected_items_only = true;
    }

    pub fn begin(&mut self, source: &Location) {
        self.active = self.index.get(source).copied();
        self.active_source = self.active.map(|_| source.clone());
    }

    pub fn clear_active(&mut self) {
        self.active = None;
        self.active_source = None;
        self.unrecorded_metadata_warning = false;
    }

    pub(crate) fn note_metadata(&mut self, source: &Location, warning: Option<MetadataWarning>) {
        let index = if self.active_source.as_ref() == Some(source) {
            self.active
        } else {
            self.nested_index.get(source).copied()
        };
        let Some(index) = index else {
            self.unrecorded_metadata_warning |= warning.is_some();
            return;
        };
        let old = self.items[index].metadata_warning.take();
        self.items[index].metadata_warning = if let Some(warning) = warning {
            Some(self.field(&crate::i18n::metadata_warning(&warning), old))
        } else {
            self.weight = self
                .weight
                .saturating_sub(old.as_deref().map_or(0, json_weight));
            None
        };
    }

    pub(crate) fn active_metadata_warning(&self) -> bool {
        self.unrecorded_metadata_warning
            || self
                .active
                .is_some_and(|index| self.items[index].metadata_warning.is_some())
            || self.group.as_ref().is_some_and(|(_, range)| {
                self.items[range.clone()]
                    .iter()
                    .any(|item| item.metadata_warning.is_some())
            })
    }

    fn add_row(
        &mut self,
        source: &Location,
        target: Option<String>,
        parent: Option<usize>,
    ) -> Option<usize> {
        self.add_display_row(&source.display(), target, parent)
    }

    fn add_display_row(
        &mut self,
        source: &str,
        target: Option<String>,
        parent: Option<usize>,
    ) -> Option<usize> {
        if self.items.len() >= KEEP_ROWS {
            self.omitted = self.omitted.saturating_add(1);
            return None;
        }
        let mut row = ItemResult::new(
            limited_text(source, MAX_TEXT_BYTES, &mut self.details_truncated),
            target.map(|text| limited_text(&text, MAX_TEXT_BYTES, &mut self.details_truncated)),
        );
        row.parent = parent;
        let weight = row.weight();
        if self.weight.saturating_add(weight) > MAX_RECORD_WEIGHT {
            self.omitted = self.omitted.saturating_add(1);
            return None;
        }
        self.weight += weight;
        let index = self.items.len();
        self.items.push(row);
        Some(index)
    }

    /// Record a diagnostic without deduplicating member names.
    pub(crate) fn archive_diagnostic(
        &mut self,
        archive: &Location,
        entry: usize,
        name: Option<&str>,
        outcome: Outcome,
        error: &std::io::Error,
    ) {
        let name = name.unwrap_or_else(|| crate::i18n::text!("Заголовок TAR"));
        let name = crate::tr_format!("Entry {index} · {name}", index = entry, name = name);
        let parent = self.index.get(archive).copied();
        if let Some(index) = self.add_display_row(&name, None, parent) {
            self.items[index].outcome = outcome;
            self.reason(index, Some(&error.to_string()));
        }
    }

    pub(crate) fn finish_diagnostics(&mut self, archive: &Location) {
        if let Some(&index) = self.index.get(archive) {
            self.items[index].descendants_enumerated = true;
        }
    }

    /// Expand an already captured manifest without performing any extra I/O.
    pub fn descendants(
        &mut self,
        source: &Location,
        target: &Location,
        entries: impl Iterator<Item = Location>,
    ) {
        self.capturing(source, target, source);
        for entry in entries {
            self.capturing(source, target, &entry);
        }
        self.captured();
    }

    /// Observe the existing source-manifest walk without extending it. These
    /// rows remain unstarted until copying begins; traversal alone publishes nothing.
    pub fn capturing(&mut self, source: &Location, target: &Location, entry: &Location) {
        let Ok(relative) = entry.path().strip_prefix(source.path()) else {
            return;
        };
        self.capturing_target(source, entry, &target.join(relative));
    }

    pub fn capturing_target(&mut self, source: &Location, entry: &Location, target: &Location) {
        let (Some(root), Some(original)) = (self.active, self.active_source.clone()) else {
            return;
        };
        if !entry.same_backend(source) {
            return;
        }
        let Ok(relative) = entry.path().strip_prefix(source.path()) else {
            return;
        };
        if self.group.is_none() {
            let start = self.items.len();
            self.group = Some((root, start..start));
            self.observed_group = true;
            self.nested_index.clear();
        }
        if relative.as_os_str().is_empty() || self.nested_index.contains_key(entry) {
            return;
        }
        if let Some(index) =
            self.add_row(&original.join(relative), Some(target.display()), Some(root))
        {
            self.nested_index.insert(entry.clone(), index);
            if let Some((_, range)) = &mut self.group {
                range.end = self.items.len();
            }
        }
    }

    /// Only a fully validated manifest establishes coverage. Publication remains
    /// pending and uses the same root acknowledgement as ordinary copy results.
    pub fn captured(&mut self) {
        if let Some(root) = self.active {
            self.items[root].descendants_enumerated = true;
            self.observed_group = false;
        }
    }

    /// Keep the operation usable when optional descendant evidence cannot be
    /// gathered. Only the root remains; the history explicitly lacks coverage.
    pub fn abandon_group(&mut self) {
        if let Some((root, range)) = self.group.take() {
            self.items.truncate(range.start);
            self.items[root].descendants_enumerated = false;
            self.weight = self.items.iter().map(ItemResult::weight).sum();
        }
        self.observed_group = false;
        self.confirming_move = false;
        self.nested_index.clear();
    }

    /// Record only entries already visited by recursive removal. No extra
    /// traversal is needed, and an interrupted walk never claims full coverage.
    pub fn removing(&mut self, source: &Location) {
        let (Some(root), Some(original)) = (self.active, self.active_source.clone()) else {
            return;
        };
        if !source.same_backend(&original) {
            return;
        }
        let Ok(relative) = source.path().strip_prefix(original.path()) else {
            return;
        };
        if self.group.is_none() {
            let start = self.items.len();
            self.group = Some((root, start..start));
            self.observed_group = true;
            self.nested_index.clear();
        }
        if !self.observed_group
            || relative.as_os_str().is_empty()
            || self.nested_index.contains_key(source)
        {
            return;
        }
        if let Some(index) = self.add_row(source, None, Some(root)) {
            self.nested_index.insert(source.clone(), index);
            if let Some((_, range)) = &mut self.group {
                range.end = self.items.len();
            }
        }
    }

    /// A confirmed deletion remains completed if a later sibling or ancestor
    /// fails. Preserve uncertainty when the server did not return a receipt.
    pub fn removed(&mut self, source: &Location, result: &Result<(), TransferError>) {
        let outcome = match result {
            Ok(()) => Outcome::Completed,
            Err(TransferError::Cancelled) => Outcome::Cancelled,
            Err(TransferError::PublicationUnconfirmed(_)) => Outcome::PublicationUnconfirmed,
            Err(TransferError::Failed(_)) => Outcome::Failed,
        };
        if self.active_source.as_ref() == Some(source) {
            self.record_with_error(source, outcome, result.as_ref().err());
        } else if self.observed_group
            && let Some(&index) = self.nested_index.get(source)
        {
            self.items[index].outcome = outcome;
            let reason = result.as_ref().err().map(crate::i18n::file_error);
            self.reason(index, reason.as_deref());
        }
    }

    /// The OS trash API acknowledges the selected root, not individual child
    /// paths. Keep observed descendants visible without inventing receipts.
    pub fn trash_descendants_unconfirmed(&mut self) {
        if let Some((_, range)) = self.group.clone() {
            let reason = crate::tr_format!(
                "The root was moved to Trash; this descendant has no separate confirmation"
            );
            for index in range {
                self.items[index].outcome = Outcome::PublicationUnconfirmed;
                self.reason(index, Some(&reason));
            }
        }
    }

    /// A successful private write remains unconfirmed until the root publishes.
    pub fn staged(&mut self, source: &Location, error: Option<String>, cancelled: bool) {
        if let Some(&index) = self.nested_index.get(source) {
            self.items[index].outcome = if cancelled {
                Outcome::Cancelled
            } else if error.is_some() {
                Outcome::Failed
            } else {
                Outcome::PublicationUnconfirmed
            };
            self.reason(index, error.as_deref());
        }
    }

    /// A root rename does not itself establish each descendant's identity.
    /// Confirm an observed path only after checking the published target.
    pub fn moved_entry(&mut self, source: &Location, verified: bool, reason: Option<String>) {
        self.confirming_move = true;
        if let Some(&index) = self.nested_index.get(source) {
            self.items[index].outcome = if verified {
                Outcome::Completed
            } else {
                Outcome::PublicationUnconfirmed
            };
            self.reason(index, reason.as_deref());
        }
    }

    pub(crate) fn moved_entry_with_error(
        &mut self,
        source: &Location,
        error: Option<&TransferError>,
    ) {
        self.moved_entry(source, error.is_none(), error.map(crate::i18n::file_error));
    }

    pub(crate) fn unconfirmed_move_descendants(&mut self) {
        if let Some((_, range)) = self.group.clone() {
            for row in &mut self.items[range] {
                if row.outcome == Outcome::NotStarted {
                    row.outcome = Outcome::PublicationUnconfirmed;
                }
            }
        }
    }

    /// A verified copy alone does not establish removal of its source.
    pub fn copied_source_remaining(&mut self) {
        self.confirming_move = true;
        if let Some((_, range)) = self.group.clone() {
            for index in range {
                self.items[index].outcome = Outcome::CopiedSourceRemaining;
                self.reason(index, None);
            }
        }
    }

    fn field(&mut self, text: &str, old: Option<String>) -> String {
        self.weight = self
            .weight
            .saturating_sub(old.as_deref().map_or(0, json_weight));
        let available = MAX_RECORD_WEIGHT.saturating_sub(self.weight);
        let mut value = limited_text(text, MAX_TEXT_BYTES, &mut self.details_truncated);
        if json_weight(&value) > available {
            value = limited_text(text, available / 6, &mut self.details_truncated);
        }
        self.weight += json_weight(&value);
        value
    }

    fn reason(&mut self, index: usize, text: Option<&str>) {
        let old = self.items[index].reason.take();
        self.items[index].reason = match text {
            Some(text) => Some(self.field(text, old)),
            None => {
                self.weight = self
                    .weight
                    .saturating_sub(old.as_deref().map_or(0, json_weight));
                None
            }
        };
    }

    fn finish_group(&mut self, root: usize, outcome: Outcome) {
        if let Some((owner, range)) = self.group.take()
            && owner == root
        {
            if self.observed_group
                && matches!(outcome, Outcome::Completed | Outcome::CompletedWithWarning)
            {
                self.items[root].descendants_enumerated = true;
            }
            for index in range {
                if !self.observed_group
                    && matches!(outcome, Outcome::Completed | Outcome::CompletedWithWarning)
                    && (!self.confirming_move || self.items[index].outcome == Outcome::NotStarted)
                {
                    self.items[index].outcome = Outcome::Completed;
                    self.reason(index, None);
                }
                if self.items[index].outcome == Outcome::Completed
                    && self.items[index].metadata_warning.is_some()
                {
                    self.items[index].outcome = Outcome::CompletedWithWarning;
                }
            }
        }
        self.observed_group = false;
        self.confirming_move = false;
        self.nested_index.clear();
    }

    pub fn target(&mut self, source: &Location, target: String) {
        if let Some(&index) = self.index.get(source) {
            let old = self.items[index].target.take();
            self.items[index].target = Some(self.field(&target, old));
        }
    }

    /// Preserve local selections already folded into this item by planning.
    /// Skipping requires no descendant walk and proves no filesystem outcome.
    pub fn skip_selected(&mut self, source: &Location, target: String, covered: &mut Vec<PathBuf>) {
        if let Some(&root) = self.index.get(source) {
            // The skipped item's existing selection buffer is no longer used
            // for a transfer. Deduplicate it in place so even omitted rows are
            // counted once, without retaining an unbounded set of paths.
            covered.sort_unstable();
            covered.dedup();
            for path in covered {
                let entry = Location::Local(path.clone());
                if entry == *source {
                    continue;
                }
                // The selected spelling can use a parent alias. Do not guess a
                // relative target or imply that every descendant was visited.
                if let Some(index) = self.add_row(&entry, None, Some(root)) {
                    self.items[index].outcome = Outcome::Skipped;
                    self.reason(
                        index,
                        Some(crate::i18n::text!(
                            "Пропущено вместе с выбранным элементом; отдельный перенос не начинался",
                        )),
                    );
                }
            }
        }
        self.record(
            source,
            Some(target),
            Outcome::Skipped,
            Some(crate::i18n::text!("Пропущено при разрешении конфликта").into()),
        );
    }

    pub fn record(
        &mut self,
        source: &Location,
        target: Option<String>,
        outcome: Outcome,
        reason: Option<String>,
    ) {
        if let Some(&index) = self.index.get(source) {
            if let Some(target) = target {
                self.target(source, target);
            }
            self.items[index].outcome = outcome;
            self.reason(index, reason.as_deref());
            self.finish_group(index, outcome);
        }
        self.clear_active();
    }

    pub(crate) fn record_with_error(
        &mut self,
        source: &Location,
        outcome: Outcome,
        error: Option<&TransferError>,
    ) {
        self.record(source, None, outcome, error.map(crate::i18n::file_error));
    }

    pub(crate) fn record_checksum(&mut self, source: &Location, outcome: ChecksumOutcome) {
        use crate::i18n::text;

        let (status, reason) = match outcome {
            ChecksumOutcome::Matched => (Outcome::Completed, text!("SHA-256 совпала")),
            ChecksumOutcome::Changed => {
                (Outcome::Failed, text!("Файл изменён (SHA-256 не совпала)"))
            }
            ChecksumOutcome::Missing => (Outcome::Failed, text!("Файл отсутствует")),
            ChecksumOutcome::Unreadable(error) => {
                self.record_with_error(source, Outcome::Failed, Some(&error));
                return;
            }
        };
        self.record(source, None, status, Some(reason.into()));
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
        let reason = result.as_ref().err().map(String::as_str).or_else(|| {
            self.cancelled
                .then(|| crate::i18n::text!("Операция остановлена пользователем"))
        });
        if let Some(index) = self.active {
            let outcome = if result.is_err() {
                Outcome::Failed
            } else if self.cancelled {
                Outcome::Cancelled
            } else {
                Outcome::Completed
            };
            self.items[index].outcome = outcome;
            self.reason(index, reason);
            self.finish_group(index, outcome);
        }
        for index in 0..self.items.len() {
            if self.items[index].outcome == Outcome::NotStarted {
                let reason = if result.is_err() {
                    Some(crate::i18n::text!(
                        "Не начато после ошибки; причина указана в итоге операции",
                    ))
                } else if self.cancelled {
                    reason
                } else {
                    None
                };
                self.reason(index, reason);
            }
        }
        Some(Record {
            id: self.id,
            label: limited_text(&label, MAX_TEXT_BYTES, &mut self.details_truncated),
            started,
            finished: now(),
            failed: result.is_err(),
            cancelled: self.cancelled,
            message: limited_text(
                result.as_ref().unwrap_or_else(|error| error),
                MAX_TEXT_BYTES,
                &mut self.details_truncated,
            ),
            items: self.items,
            selected_items_only: self.selected_items_only,
            omitted: self.omitted,
            details_truncated: self.details_truncated,
        })
    }
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn operation_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Record {
    #[serde(default)]
    id: String,
    label: String,
    started: u64,
    finished: u64,
    failed: bool,
    cancelled: bool,
    message: String,
    pub items: Vec<ItemResult>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    selected_items_only: bool,
    #[serde(default)]
    omitted: usize,
    #[serde(default)]
    details_truncated: bool,
}

impl ItemResult {
    fn weight(&self) -> usize {
        ROW_WEIGHT
            + json_weight(&self.source)
            + self.target.as_deref().map_or(0, json_weight)
            + self.reason.as_deref().map_or(0, json_weight)
            + self.metadata_warning.as_deref().map_or(0, json_weight)
    }
}

impl Record {
    fn selected_items_only(&self) -> bool {
        // Older checksum and permission records stored only the translated command label.
        self.selected_items_only
            || crate::i18n::key!("Создать манифест SHA-256").matches(&self.label)
            || crate::i18n::key!("Проверить манифест SHA-256").matches(&self.label)
            || (crate::i18n::key!("Изменить права доступа").matches(&self.label)
                && self.items.iter().all(|row| {
                    row.parent.is_none()
                        && row.target.is_none()
                        && row.source.starts_with("sftp://")
                }))
    }

    fn weight(&self) -> usize {
        4096 + json_weight(&self.id)
            + json_weight(&self.label)
            + json_weight(&self.message)
            + self.items.iter().map(ItemResult::weight).sum::<usize>()
    }

    fn limit(&mut self) {
        self.id = limited_text(&self.id, 128, &mut self.details_truncated);
        self.label = limited_text(&self.label, MAX_TEXT_BYTES, &mut self.details_truncated);
        self.message = limited_text(&self.message, MAX_TEXT_BYTES, &mut self.details_truncated);
        let mut weight =
            4096 + json_weight(&self.id) + json_weight(&self.label) + json_weight(&self.message);
        let mut keep = 0;
        for row in &self.items {
            if keep == KEEP_ROWS || weight.saturating_add(row.weight()) > MAX_RECORD_WEIGHT {
                break;
            }
            weight += row.weight();
            keep += 1;
        }
        self.omitted = self.omitted.saturating_add(self.items.len() - keep);
        self.items.truncate(keep);
    }
}

fn json_weight(text: &str) -> usize {
    text.bytes()
        .map(|byte| match byte {
            b'"' | b'\\' => 2,
            0..=31 => 6,
            _ => 1,
        })
        .sum()
}

fn limited_text(text: &str, maximum: usize, truncated: &mut bool) -> String {
    if text.len() <= maximum {
        return text.to_owned();
    }
    *truncated = true;
    let suffix = "…";
    if maximum < suffix.len() {
        return String::new();
    }
    let mut end = maximum - suffix.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &text[..end])
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    version: u32,
    records: Vec<Record>,
}

impl Saved {
    fn push(&mut self, mut record: Record) {
        record.limit();
        self.records.push(record);
        let mut rows = 0;
        let mut weight: usize = 0;
        let mut retain = self.records.len();
        for record in self.records.iter().rev().take(KEEP_OPERATIONS) {
            if retain < self.records.len()
                && (rows + record.items.len() > KEEP_ROWS
                    || weight.saturating_add(record.weight()) > MAX_BYTES as usize - 4096)
            {
                break;
            }
            rows += record.items.len();
            weight += record.weight();
            retain -= 1;
        }
        self.records.drain(..retain);
    }
}

struct Update {
    records: Vec<Record>,
    error: Option<Arc<HistoryError>>,
}

#[derive(Debug)]
pub(crate) enum HistoryError {
    Unavailable,
    Disconnected(PathBuf),
    Spawn {
        path: PathBuf,
        source: io::Error,
    },
    Open {
        path: PathBuf,
        problem: HistoryOpenProblem,
    },
    Save {
        path: PathBuf,
        problem: HistorySaveProblem,
    },
}

#[derive(Debug)]
pub(crate) enum HistoryOpenProblem {
    MissingParent,
    CreateDirectory(io::Error),
    OpenLock(io::Error),
    Locked,
    Lock(io::Error),
    Read(io::Error),
    InvalidType,
    IdentityChanged,
    SizeLimit(u64),
    Json(serde_json::Error),
    UnsupportedVersion(u32),
}

#[derive(Debug)]
pub(crate) enum HistorySaveProblem {
    MissingParent,
    Serialize(serde_json::Error),
    SizeLimit(u64),
    CreateTemporary(io::Error),
    WriteTemporary(io::Error),
    SyncTemporary(io::Error),
    CheckLock(io::Error),
    LockChanged,
    Replace(io::Error),
    ConfirmReplacement(io::Error),
}

impl HistoryError {
    fn lost_lock(&self) -> bool {
        match self {
            Self::Save {
                problem: HistorySaveProblem::LockChanged,
                ..
            } => true,
            Self::Save {
                problem: HistorySaveProblem::CheckLock(source),
                ..
            } => source.kind() == io::ErrorKind::NotFound,
            _ => false,
        }
    }
}

struct Append {
    record: Record,
    saved: Sender<Result<(), Arc<HistoryError>>>,
}

#[derive(Clone, Default)]
enum HistoryDelivery {
    #[default]
    Unavailable,
    Failed(Arc<HistoryError>),
    Connected {
        path: PathBuf,
        requests: Sender<Append>,
    },
}

#[derive(Clone, Default)]
pub(crate) struct HistorySender(HistoryDelivery);

impl HistorySender {
    /// Acknowledge only after the new history has been saved and confirmed.
    /// Recovery markers must stay available when confirmation fails.
    pub fn append(&self, record: Record) -> Result<(), Arc<HistoryError>> {
        match &self.0 {
            HistoryDelivery::Unavailable => Err(Arc::new(HistoryError::Unavailable)),
            HistoryDelivery::Failed(error) => Err(error.clone()),
            HistoryDelivery::Connected { path, requests } => {
                let disconnected = || Arc::new(HistoryError::Disconnected(path.clone()));
                let (saved, wait) = mpsc::channel();
                requests
                    .send(Append { record, saved })
                    .map_err(|_| disconnected())?;
                wait.recv().map_err(|_| disconnected())?
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct History {
    sender: HistorySender,
    updates: Option<Receiver<Update>>,
    records: Vec<Record>,
    pub error: Option<Arc<HistoryError>>,
    path: Option<PathBuf>,
    loaded: bool,
    detail: Option<ItemResult>,
    pub retryable: HashSet<String>,
    pub inspectable: HashSet<String>,
    pub retry_requested: Option<String>,
    pub undoable: HashMap<String, usize>,
    pub undo_requested: Option<String>,
}

#[cfg(target_os = "macos")]
pub(crate) struct DiagnosticFailure<'a> {
    pub(crate) finished: u64,
    pub(crate) label: &'a str,
    pub(crate) message: &'a str,
    pub(crate) reason: Option<&'a str>,
}

impl History {
    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf, ctx: &egui::Context) -> Self {
        Self::open(path, ctx)
    }

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
                error: Some(Arc::new(HistoryError::Unavailable)),
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
                let (mut saved, mut lock, mut error) = match opened {
                    Ok((saved, lock)) => (saved, Some(lock), None),
                    Err(error) => (
                        Saved {
                            version: VERSION,
                            ..Saved::default()
                        },
                        None,
                        Some(Arc::new(error)),
                    ),
                };
                let _ = updates.send(Update {
                    records: saved.records.clone(),
                    error: error.clone(),
                });
                ctx.request_repaint();
                for request in requests {
                    saved.push(request.record);
                    if let Some(owned) = &lock {
                        error = save(&worker_path, &saved, owned).err().map(Arc::new);
                        if error.as_deref().is_some_and(HistoryError::lost_lock) {
                            // Another writer may have published history under a different lock.
                            lock = None;
                        }
                    }
                    let _ = updates.send(Update {
                        records: saved.records.clone(),
                        error: error.clone(),
                    });
                    ctx.request_repaint();
                    let _ = request
                        .saved
                        .send(error.as_ref().map_or(Ok(()), |error| Err(error.clone())));
                }
            });
        match worker {
            Ok(_) => Self {
                sender: HistorySender(HistoryDelivery::Connected {
                    path: path.clone(),
                    requests: send,
                }),
                updates: Some(receive),
                path: Some(path),
                ..Self::default()
            },
            Err(source) => {
                let error = Arc::new(HistoryError::Spawn {
                    path: path.clone(),
                    source,
                });
                Self {
                    error: Some(error.clone()),
                    sender: HistorySender(HistoryDelivery::Failed(error)),
                    path: Some(path),
                    ..Self::default()
                }
            }
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
                        self.error = self
                            .path
                            .as_ref()
                            .map(|path| Arc::new(HistoryError::Disconnected(path.clone())));
                        break;
                    }
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn diagnostics_loaded(&self) -> bool {
        self.loaded
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn diagnostic_failures(&self) -> impl Iterator<Item = DiagnosticFailure<'_>> {
        self.records.iter().rev().filter_map(|record| {
            let failed_item = record.items.iter().find(|item| {
                matches!(
                    item.outcome,
                    Outcome::Failed
                        | Outcome::PublicationUnconfirmed
                        | Outcome::CopiedSourceRemaining
                )
            });
            ((record.failed && !record.cancelled) || failed_item.is_some()).then(|| {
                DiagnosticFailure {
                    finished: record.finished,
                    label: &record.label,
                    message: &record.message,
                    reason: failed_item.and_then(|item| item.reason.as_deref()),
                }
            })
        })
    }

    pub fn show(&mut self, ctx: &egui::Context, open: &mut bool) {
        let mut selected = None;
        egui::Window::new(crate::i18n::text!("Журнал результатов"))
            .id(egui::Id::unique("result_history"))
            .open(open)
            .default_size([780.0, 560.0])
            .show(ctx, |ui| {
                ui.label(crate::tr_format!(
                    "Copies expand from their manifest; SFTP removal lists visited entries. Incomplete traversal is marked."
                ));
                ui.label(crate::i18n::text!(
                    "До 100 операций и 100 000 строк; объём истории ограничен. Неполные записи помечены.",
                ));
                ui.label(crate::tr_format!(
                    "Choose Details to read the complete paths and reason."
                ));
                ui.label(crate::tr_format!(
                    "For interrupted SFTP transfers, inspection shows the state of the source, destination and retained temporary files."
                ));
                ui.label(crate::tr_format!(
                    "Undo reverses confirmed renames and moves from this session without overwriting. Replacements, copies, deletions, unverified items and operations from before restart are unavailable. Files and folders are checked before restoring."
                ));
                if let Some(path) = &self.path {
                    ui.add(egui::Label::new(path.display().to_string()).selectable(true));
                }
                if let Some(error) = &self.error {
                    let message = egui::RichText::new(crate::i18n::history_error(error))
                        .color(ui.visuals().error_fg_color);
                    ui.add(egui::Label::new(message).selectable(true));
                }
                if self.records.is_empty() {
                    ui.label(if self.loaded {
                        crate::i18n::text!("Журнал пуст")
                    } else {
                        crate::i18n::text!("Загрузка журнала…")
                    });
                }
                egui::ScrollArea::vertical()
                    .id_salt("result_operations")
                    .show(ui, |ui| {
                        for (index, record) in self.records.iter().enumerate().rev() {
                            let show_descendant_coverage = !record.selected_items_only();
                            let time = chrono::DateTime::from_timestamp(record.started as i64, 0)
                                .map(|time| {
                                    crate::i18n::display::date(time.with_timezone(&chrono::Local), crate::i18n::display::DateStyle::Seconds)
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
                                if let Some(count) = self.undoable.get(&record.id) {
                                    if ui.button(crate::tr_format!("Undo available changes ({count})", count = count)).clicked() {
                                        self.undo_requested = Some(record.id.clone());
                                    }
                                } else {
                                    ui.label(crate::i18n::text!("Undo недоступен для этой операции"));
                                }
                                if self.retryable.contains(&record.id) && ui.button(crate::tr_format!("Check and retry unfinished items")).clicked() {
                                    self.retry_requested = Some(record.id.clone());
                                }
                                if self.inspectable.contains(&record.id)
                                    && ui.button(crate::i18n::text!("Проверить прерванную операцию SFTP")).clicked()
                                {
                                    self.retry_requested = Some(record.id.clone());
                                }
                                if record.omitted != 0 {
                                    ui.colored_label(ui.visuals().warn_fg_color, crate::tr_format!(
                                        "Rows omitted by history limits: {}. This does not change the transfer outcome.", record.omitted));
                                }
                                if record.details_truncated {
                                    ui.colored_label(ui.visuals().warn_fg_color, crate::i18n::text!("Длинные пути или сообщения сокращены из-за лимита журнала."));
                                }
                                // Use the laid-out three-line height: fractional font metrics
                                // can otherwise underestimate each virtual row and clip the end.
                                let row_height = ui.painter().layout_no_wrap(
                                    "\n\n".into(),
                                    egui::TextStyle::Body.resolve(ui.style()),
                                    ui.visuals().text_color(),
                                ).size().y;
                                egui::ScrollArea::both()
                                    .id_salt(("result_items", index))
                                    // Keep rounded glyph bounds inside the clip at fractional zoom.
                                    .content_margin(egui::Margin::same(1))
                                    .max_height(360.0)
                                    .show_rows(
                                        ui,
                                        row_height,
                                        record.items.len(),
                                        |ui, range| {
                                            for row in &record.items[range] {
                                                ui.horizontal(|ui| {
                                                if ui.small_button(crate::tr_format!("Details")).clicked() {
                                                    selected = Some(row.clone());
                                                }
                                                let source = single_line(&row.source);
                                                let paths = if let Some(target) = &row.target {
                                                    format!("{source} → {}", single_line(target))
                                                } else {
                                                    source
                                                };
                                                ui.add(
                                                    egui::Label::new(format!(
                                                        "{}\n{paths}\n{}",
                                                        if show_descendant_coverage && row.parent.is_none() && !row.descendants_enumerated { format!("{} · {}", row.outcome.label(), crate::tr_format!("Descendants were not fully listed")) } else { row.outcome.label().to_owned() },
                                                        single_line(
                                                            row.reason.as_deref().or(row.metadata_warning.as_deref()).unwrap_or("—")
                                                        )
                                                    ))
                                                    .selectable(true)
                                                    .extend(),
                                                )
                                                .on_hover_ui(|ui| {
                                                    ui.set_max_width(640.0);
                                                    egui::ScrollArea::vertical()
                                                        .max_height(320.0)
                                                        .show(ui, |ui| show_item_details(ui, row));
                                                });
                                                });
                                            }
                                        },
                                    );
                            });
                        }
                    });
            });
        if let Some(row) = selected {
            self.detail = Some(row);
        }
        if !*open {
            self.detail = None;
        }
        if let Some(row) = &self.detail {
            let mut detail_open = true;
            egui::Window::new(crate::tr_format!("File result"))
                .id(egui::Id::unique("result_history_detail"))
                .open(&mut detail_open)
                .default_size([640.0, 360.0])
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| show_item_details(ui, row));
                });
            if !detail_open {
                self.detail = None;
            }
        }
    }
}

fn show_item_details(ui: &mut egui::Ui, row: &ItemResult) {
    ui.label(row.outcome.label());
    ui.add(
        egui::Label::new(crate::tr_format!("Source: {}", row.source))
            .selectable(true)
            .wrap(),
    );
    ui.add(
        egui::Label::new(crate::tr_format!(
            "Destination: {}",
            row.target.as_deref().unwrap_or("—")
        ))
        .selectable(true)
        .wrap(),
    );
    ui.add(
        egui::Label::new(crate::tr_format!(
            "Reason: {}",
            row.reason.as_deref().unwrap_or("—")
        ))
        .selectable(true)
        .wrap(),
    );
    if let Some(warning) = &row.metadata_warning {
        ui.add(egui::Label::new(warning).selectable(true).wrap());
    }
}

fn single_line(text: &str) -> String {
    text.replace('\r', "\\r").replace('\n', "\\n")
}

fn open_store(path: &Path) -> Result<(Saved, File), HistoryError> {
    let open_error = |problem| HistoryError::Open {
        path: path.to_owned(),
        problem,
    };
    let parent = path
        .parent()
        .ok_or_else(|| open_error(HistoryOpenProblem::MissingParent))?;
    fs::create_dir_all(parent).map_err(|source| HistoryError::Open {
        path: parent.to_owned(),
        problem: HistoryOpenProblem::CreateDirectory(source),
    })?;
    let lock_path = path.with_extension("lock");
    let lock_error = |problem| HistoryError::Open {
        path: lock_path.clone(),
        problem,
    };
    let lock = open_history_lock(&lock_path).map_err(lock_error)?;
    let saved = read_history(path).map_err(open_error)?;
    let mut bounded = Saved {
        version: VERSION,
        records: Vec::new(),
    };
    for record in saved.records {
        bounded.push(record);
    }
    if !same_owned_marker(&lock, &lock_path)
        .map_err(|source| lock_error(HistoryOpenProblem::OpenLock(source)))?
    {
        return Err(lock_error(HistoryOpenProblem::IdentityChanged));
    }
    Ok((bounded, lock))
}

fn history_file_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // A replacement between inspection and open must not redirect or block the worker.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options
}

fn regular_history_file_exists(
    path: &Path,
    on_io: fn(io::Error) -> HistoryOpenProblem,
) -> Result<bool, HistoryOpenProblem> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(HistoryOpenProblem::InvalidType),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(on_io(error)),
    }
}

fn open_history_lock(path: &Path) -> Result<File, HistoryOpenProblem> {
    let open_lock = HistoryOpenProblem::OpenLock;
    regular_history_file_exists(path, open_lock)?;
    let lock = history_file_options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(open_lock)?;
    if !lock.metadata().map_err(open_lock)?.is_file() {
        return Err(HistoryOpenProblem::InvalidType);
    }
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(HistoryOpenProblem::Locked),
        Err(TryLockError::Error(source)) => return Err(HistoryOpenProblem::Lock(source)),
    }
    Ok(lock)
}

fn read_history(path: &Path) -> Result<Saved, HistoryOpenProblem> {
    let read = HistoryOpenProblem::Read;
    if !regular_history_file_exists(path, read)? {
        return Ok(Saved {
            version: VERSION,
            ..Saved::default()
        });
    }
    let file = history_file_options().open(path).map_err(read)?;
    let metadata = file.metadata().map_err(read)?;
    if !metadata.is_file() {
        return Err(HistoryOpenProblem::InvalidType);
    }
    if metadata.len() > MAX_BYTES {
        return Err(HistoryOpenProblem::SizeLimit(MAX_BYTES));
    }
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(read)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(HistoryOpenProblem::SizeLimit(MAX_BYTES));
    }
    if !same_owned_marker(&file, path).map_err(read)? {
        return Err(HistoryOpenProblem::IdentityChanged);
    }

    #[derive(Deserialize)]
    struct StoredVersion {
        version: u32,
    }

    match serde_json::from_slice::<Saved>(&bytes) {
        Ok(saved) if saved.version == VERSION => Ok(saved),
        Ok(saved) => Err(HistoryOpenProblem::UnsupportedVersion(saved.version)),
        // Future versions may have fields that no longer deserialize as Saved.
        Err(source) => match serde_json::from_slice::<StoredVersion>(&bytes) {
            Ok(header) if header.version != VERSION => {
                Err(HistoryOpenProblem::UnsupportedVersion(header.version))
            }
            _ => Err(HistoryOpenProblem::Json(source)),
        },
    }
}

fn save(path: &Path, saved: &Saved, lock: &File) -> Result<(), HistoryError> {
    use HistorySaveProblem::*;

    let failure = |problem| HistoryError::Save {
        path: path.to_owned(),
        problem,
    };
    let bytes = serde_json::to_vec(saved).map_err(|source| failure(Serialize(source)))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(failure(SizeLimit(MAX_BYTES)));
    }
    let parent = path.parent().ok_or_else(|| failure(MissingParent))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .map_err(|source| failure(CreateTemporary(source)))?;
    file.write_all(&bytes)
        .map_err(|source| failure(WriteTemporary(source)))?;
    file.as_file()
        .sync_all()
        .map_err(|source| failure(SyncTemporary(source)))?;

    let lock_path = path.with_extension("lock");
    let lock_error = |problem| HistoryError::Save {
        path: lock_path.clone(),
        problem,
    };
    if !same_owned_marker(lock, &lock_path).map_err(|source| lock_error(CheckLock(source)))? {
        return Err(lock_error(LockChanged));
    }
    file.persist(path)
        .map_err(|error| failure(Replace(error.error)))?;
    // Recovery may forget its pending marker only after the rename is durable.
    sync_directory(parent).map_err(|source| failure(ConfirmReplacement(source)))
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
    fn permission_history_reports_only_selected_items_in_new_and_legacy_records() {
        let source = crate::vfs::resolve("sftp://user@example.test/folder", &local("/")).unwrap();
        let mut results = Results::default();
        results.plan_selected_items([source.clone()]);
        results.record(
            &source,
            None,
            Outcome::Failed,
            Some("permission denied".into()),
        );
        let current = results
            .finish(
                "Current command label".into(),
                0,
                &Err("permission denied".into()),
            )
            .unwrap();
        let value = serde_json::to_value(&current).unwrap();
        let restored: Record = serde_json::from_value(value.clone()).unwrap();
        assert!(restored.selected_items_only());
        assert!(!restored.items[0].descendants_enumerated);
        assert_eq!(restored.items[0].outcome, Outcome::Failed);

        for label in ["Change permissions", "Изменить права доступа"] {
            let mut legacy = value.clone();
            legacy
                .as_object_mut()
                .unwrap()
                .remove("selected_items_only");
            legacy["label"] = label.into();
            let restored: Record = serde_json::from_value(legacy).unwrap();
            assert!(restored.selected_items_only());
        }
        let mut copy = record();
        copy.label = "Change permissions".into();
        assert!(!copy.selected_items_only());
    }

    #[test]
    fn checksum_history_recognizes_legacy_manifest_commands_in_all_languages() {
        let mut results = Results::default();
        results.plan([(local("SHA256SUMS"), None)]);
        let mut record = results
            .finish(String::new(), 0, &Ok(String::new()))
            .unwrap();
        for label in [
            "Create SHA-256 manifest",
            "Создать манифест SHA-256",
            "Verify SHA-256 manifest",
            "Проверить манифест SHA-256",
            #[cfg(target_os = "macos")]
            "Vytvořit manifest SHA-256",
            #[cfg(target_os = "macos")]
            "Ověřit manifest SHA-256",
        ] {
            record.label = label.into();
            let saved = serde_json::to_value(&record).unwrap();
            assert!(saved.get("selected_items_only").is_none());
            let restored: Record = serde_json::from_value(saved).unwrap();
            assert!(restored.selected_items_only(), "{label}");
        }
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
        for zoom in [1.0, 1.25, 1.5, 2.0] {
            let ctx = egui::Context::default();
            ctx.set_zoom_factor(zoom);
            let mut entry = record();
            entry.items = (0..100)
                .map(|index| ItemResult {
                    source: format!("source-{index:03}"),
                    target: Some("target\nwith newline".into()),
                    outcome: Outcome::Failed,
                    reason: Some("first line\nsecond line\r\nthird line".into()),
                    metadata_warning: None,
                    parent: None,
                    descendants_enumerated: false,
                })
                .collect();
            let mut history = History {
                records: vec![entry],
                loaded: true,
                ..History::default()
            };
            let mut open = true;
            let mut draw = |events| {
                ctx.run_test_ui(
                    egui::RawInput {
                        events,
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(960.0, 700.0),
                        )),
                        ..Default::default()
                    },
                    |ui| history.show(ui.ctx(), &mut open),
                )
            };
            draw(vec![]);
            let output = draw(vec![]);
            let mut item_bounds = Vec::new();
            for clipped in &output.shapes {
                if let egui::Shape::Text(text) = &clipped.shape
                    && text.galley.text().contains("source-")
                {
                    assert_eq!(text.galley.rows.len(), 3);
                    assert!(text.galley.text().contains("second line\\r\\nthird line"));
                    item_bounds.push(egui::Rect::from_min_size(text.pos, text.galley.size()));
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
                        phase: egui::TouchPhase::Move,
                        source: egui::MouseWheelSource::Unknown,
                        delta: egui::vec2(0.0, -10_000.0),
                        modifiers: egui::Modifiers::NONE,
                    },
                ]);
            }
            let last = output
                .shapes
                .iter()
                .find_map(|clipped| {
                    if let egui::Shape::Text(text) = &clipped.shape
                        && text.galley.text().contains("source-099")
                    {
                        return Some((clipped.clip_rect, text));
                    }
                    None
                })
                .expect("last result remains reachable by scrolling");
            assert!(
                last.0
                    .contains_rect(egui::Rect::from_min_size(last.1.pos, last.1.galley.size())),
                "last row is fully visible: clip={:?}, text={:?}",
                last.0,
                egui::Rect::from_min_size(last.1.pos, last.1.galley.size())
            );
        }
    }

    #[test]
    fn interrupted_sftp_history_offers_inspection_before_retry() {
        let ctx = egui::Context::default();
        let mut entry = record();
        entry.id = "interrupted".into();
        entry.items[0].outcome = Outcome::PublicationUnconfirmed;
        let mut history = History {
            records: vec![entry],
            loaded: true,
            inspectable: HashSet::from(["interrupted".into()]),
            ..History::default()
        };
        let mut open = true;
        let mut draw = |events| {
            ctx.run_test_ui(
                egui::RawInput {
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(960., 700.),
                    )),
                    ..Default::default()
                },
                |ui| history.show(ui.ctx(), &mut open),
            )
        };
        draw(Vec::new());
        let output = draw(Vec::new());
        let mut check_button = None;
        for clipped in output.shapes {
            if let egui::Shape::Text(label) = clipped.shape {
                assert_ne!(label.galley.text(), "Check and retry unfinished items");
                if label.galley.text() == crate::i18n::text!("Проверить прерванную операцию SFTP")
                {
                    check_button = Some(label.pos + label.galley.size() * 0.5);
                }
            }
        }
        let pos = check_button.expect("inspection button is visible");
        for pressed in [true, false] {
            draw(vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
        }
        assert_eq!(history.retry_requested.as_deref(), Some("interrupted"));
        assert!(history.retryable.is_empty());
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
            Outcome::RetryReady,
            Outcome::NotStarted,
        ] {
            item.items.push(ItemResult {
                source: "sftp://user@host:22/кириллица".into(),
                target: None,
                outcome,
                reason: Some(
                    "Check original target and staging /private/recovery\nsecond line".into(),
                ),
                metadata_warning: None,
                parent: None,
                descendants_enumerated: false,
            });
        }
        saved.push(item);
        save(&path, &saved, &lock).unwrap();
        drop(lock);
        let (reopened, _lock) = open_store(&path).unwrap();
        assert_eq!(reopened.records[0].items.len(), 7);
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
        assert!(history.sender().append(record()).is_ok());
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
        let error = history.sender().append(record()).unwrap_err();
        assert!(matches!(
            &*error,
            HistoryError::Open { path, problem: HistoryOpenProblem::Json(source) }
                if path == &corrupt && source.line() == 1 && source.column() == 2
        ));
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
        save(&path, &saved, &lock).unwrap();
        let baseline = fs::read(&path).unwrap();
        let ctx = egui::Context::default();
        let mut history = History::open(path.clone(), &ctx);
        let error = history.sender().append(record()).unwrap_err();
        assert!(matches!(
            &*error,
            HistoryError::Open { path: locked, problem: HistoryOpenProblem::Locked }
                if *locked == path.with_extension("lock")
        ));
        history.poll();
        assert!(history.error.is_some());
        assert_eq!(history.records.len(), 1);
        assert_eq!(fs::read(&path).unwrap(), baseline);
        drop(lock);
        let future = br#"{"version":999,"records":[{"future":true}]}"#;
        fs::write(&path, future).unwrap();
        let history = History::open(path.clone(), &ctx);
        let error = history.sender().append(record()).unwrap_err();
        assert!(matches!(
            &*error,
            HistoryError::Open { path: rejected, problem: HistoryOpenProblem::UnsupportedVersion(999) }
                if rejected == &path
        ));
        assert_eq!(fs::read(path).unwrap(), future);
    }

    #[test]
    fn oversized_history_is_rejected_before_deserialization() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        File::create(&path).unwrap().set_len(MAX_BYTES + 1).unwrap();
        assert!(matches!(
            open_store(&path),
            Err(HistoryError::Open { path: rejected, problem: HistoryOpenProblem::SizeLimit(limit) })
                if rejected == path && limit == MAX_BYTES
        ));
        assert_eq!(fs::metadata(path).unwrap().len(), MAX_BYTES + 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn history_open_rejects_links_and_fifos_in_both_paths() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::{symlink, FileTypeExt as _};
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let history = dir.path().join("history.json");
        let lock = history.with_extension("lock");
        let target = dir.path().join("independent-file");
        fs::write(&target, b"independent contents").unwrap();

        for path in [&history, &lock] {
            if path == &lock {
                fs::remove_file(&lock).unwrap();
            }
            symlink(&target, path).unwrap();
            assert!(matches!(
                open_store(&history),
                Err(HistoryError::Open { path: rejected, problem: HistoryOpenProblem::InvalidType })
                    if rejected == *path
            ));
            assert_eq!(fs::read_link(path).unwrap(), target);
            fs::remove_file(path).unwrap();

            let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: c_path is a live C string in the disposable test directory.
            assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
            let (send, receive) = mpsc::channel();
            let worker_path = history.clone();
            std::thread::spawn(move || {
                let _ = send.send(open_store(&worker_path));
            });
            assert!(matches!(
                receive.recv_timeout(Duration::from_secs(3)).expect("history open blocked"),
                Err(HistoryError::Open { path: rejected, problem: HistoryOpenProblem::InvalidType })
                    if rejected == *path
            ));
            assert!(fs::symlink_metadata(path).unwrap().file_type().is_fifo());
            fs::remove_file(path).unwrap();
        }
        assert_eq!(fs::read(target).unwrap(), b"independent contents");
    }

    #[test]
    fn oversized_snapshot_preserves_previous_file_and_removes_tempfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let (mut saved, lock) = open_store(&path).unwrap();
        saved.push(record());
        save(&path, &saved, &lock).unwrap();
        let baseline = fs::read(&path).unwrap();
        saved.records[0].message = "x".repeat(MAX_BYTES as usize);
        assert!(matches!(
            save(&path, &saved, &lock),
            Err(HistoryError::Save { path: failed, problem: HistorySaveProblem::SizeLimit(limit) })
                if failed == path && limit == MAX_BYTES
        ));
        assert_eq!(fs::read(&path).unwrap(), baseline);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn retention_bounds_rows_and_storage_and_marks_omitted_tail() {
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
        assert!(saved.records.len() <= KEEP_OPERATIONS);
        assert!(
            saved
                .records
                .iter()
                .map(|record| record.items.len())
                .sum::<usize>()
                <= KEEP_ROWS
        );
        assert!(
            saved.records.iter().map(Record::weight).sum::<usize>() <= MAX_BYTES as usize - 4096
        );
        assert!(saved.records.last().unwrap().omitted > 0);
        assert!(saved.records.last().unwrap().weight() <= MAX_RECORD_WEIGHT);
        saved.push(record());
        assert!(
            saved
                .records
                .iter()
                .map(|record| record.items.len())
                .sum::<usize>()
                <= KEEP_ROWS
        );
    }

    #[test]
    fn terminal_failure_does_not_reclassify_published_or_skipped_items() {
        let mut results = Results::default();
        results.plan(
            ["published", "skipped", "failed", "pending"].map(|source| (local(source), None)),
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
            Some(crate::i18n::text!(
                "Не начато после ошибки; причина указана в итоге операции"
            ))
        );
    }

    #[test]
    fn nested_collection_is_bounded_and_omissions_do_not_change_publication() {
        let selected = local("/selected-tree");
        let physical = local("/physical-tree");
        let target = local("/target");
        let mut results = Results::default();
        results.plan([(selected.clone(), Some(target.display()))]);
        results.begin(&selected);
        let count = KEEP_ROWS + 50;
        results.descendants(
            &physical,
            &target,
            (0..count).map(|index| physical.join(format!("file-{index:06}"))),
        );
        assert!(results.items.len() <= KEEP_ROWS);
        assert!(results.weight <= MAX_RECORD_WEIGHT);
        assert_eq!(results.omitted + results.items.len(), count + 1);
        assert!(results.omitted > 0);
        assert_eq!(
            results.items[1].source,
            selected.join("file-000000").display()
        );
        results.staged(&physical.join("file-000000"), None, false);
        results.record(&selected, None, Outcome::Completed, None);
        let record = results
            .finish("copy".into(), 0, &Ok("Copied".into()))
            .unwrap();
        assert!(record
            .items
            .iter()
            .all(|row| row.outcome == Outcome::Completed));
        let mut saved = Saved {
            version: VERSION,
            records: Vec::new(),
        };
        saved.push(record);
        assert!(saved.records[0].omitted > 0);
        assert!(saved.records[0].items[0].descendants_enumerated);
        assert_eq!(saved.records[0].items[0].outcome, Outcome::Completed);
        assert!(serde_json::to_vec(&saved).unwrap().len() <= MAX_BYTES as usize);
    }

    #[test]
    fn nested_publication_is_acknowledged_despite_later_root_cleanup_warning() {
        let root = local("/source");
        let child = root.join("nested/file");
        let mut results = Results::default();
        results.plan([(root.clone(), Some("/target".into()))]);
        results.begin(&root);
        results.descendants(&root, &local("/target"), [child.clone()].into_iter());
        results.staged(&child, None, false);
        assert_eq!(results.items[1].outcome, Outcome::PublicationUnconfirmed);
        results.record(
            &root,
            None,
            Outcome::CompletedWithWarning,
            Some("cleanup failed; inspect staging".into()),
        );
        let record = results
            .finish("copy".into(), 0, &Err("cleanup failed".into()))
            .unwrap();
        assert_eq!(record.items[0].outcome, Outcome::CompletedWithWarning);
        assert_eq!(record.items[1].outcome, Outcome::Completed);
        assert!(record.items[1].reason.is_none());
    }

    #[test]
    fn observed_removals_preserve_confirmed_children_after_partial_failure() {
        let root = local("/tree");
        let directory = root.join("nested");
        let child = directory.join("removed");
        let failed = root.join("denied");
        let mut results = Results::default();
        results.plan([(root.clone(), None)]);
        results.begin(&root);
        for entry in [&root, &directory, &child] {
            results.removing(entry);
        }
        results.removed(&child, &Ok(()));
        results.removed(&directory, &Ok(()));
        results.removing(&failed);
        results.removed(&failed, &Err("permission denied".into()));
        results.removed(&root, &Err("permission denied".into()));
        let record = results
            .finish("delete".into(), 0, &Err("partial removal".into()))
            .unwrap();
        assert_eq!(
            record.items.len(),
            4,
            "unvisited descendants are never invented"
        );
        assert!(!record.items[0].descendants_enumerated);
        assert_eq!(record.items[0].outcome, Outcome::Failed);
        assert_eq!(record.items[1].outcome, Outcome::Completed);
        assert_eq!(record.items[2].outcome, Outcome::Completed);
        assert_eq!(record.items[3].outcome, Outcome::Failed);
        assert_eq!(record.items[3].reason.as_deref(), Some("permission denied"));
        assert!(record.items[1..]
            .iter()
            .all(|row| row.parent == Some(0) && row.target.is_none()));
        let reopened: Record =
            serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(!reopened.items[0].descendants_enumerated);
        assert_eq!(reopened.items[2].outcome, Outcome::Completed);
    }

    #[test]
    fn successful_observed_removal_marks_coverage_and_keeps_roots_independent() {
        let first = local("/first");
        let second = local("/second");
        let child = first.join("file");
        let mut results = Results::default();
        results.plan([(first.clone(), None), (second.clone(), None)]);
        results.begin(&first);
        results.removing(&first);
        results.removing(&child);
        results.removing(&child);
        results.removing(&second.join("unrelated"));
        results.removed(&child, &Ok(()));
        results.record(&first, None, Outcome::Completed, None);
        results.begin(&second);
        results.removing(&second);
        results.record(&second, None, Outcome::Completed, None);
        let record = results
            .finish("delete".into(), 0, &Ok("deleted".into()))
            .unwrap();
        assert_eq!(record.items.len(), 3, "no duplicate or unrelated rows");
        assert!(record.items[0].descendants_enumerated);
        assert!(
            record.items[1].descendants_enumerated,
            "empty directory is fully visited"
        );
        assert_eq!(record.items[2].parent, Some(0));
        assert!(record
            .items
            .iter()
            .all(|row| row.outcome == Outcome::Completed));
    }

    #[test]
    fn observed_removal_limits_do_not_change_root_result() {
        let root = local("/tree");
        let mut results = Results::default();
        results.plan([(root.clone(), None)]);
        results.begin(&root);
        results.removing(&root);
        results.weight = MAX_RECORD_WEIGHT;
        results.removing(&root.join("not-retained"));
        results.record(&root, None, Outcome::Completed, None);
        let record = results
            .finish("delete".into(), 0, &Ok("deleted".into()))
            .unwrap();
        assert_eq!(record.items.len(), 1);
        assert_eq!(record.omitted, 1);
        assert!(record.items[0].descendants_enumerated);
        assert_eq!(record.items[0].outcome, Outcome::Completed);
    }

    #[test]
    fn trash_root_confirmation_does_not_invent_child_receipts() {
        let root = local("/tree");
        let child = root.join("nested/file");
        let mut results = Results::default();
        results.plan([(root.clone(), None)]);
        results.begin(&root);
        results.removing(&root);
        results.removing(&child);
        results.trash_descendants_unconfirmed();
        results.record(&root, None, Outcome::Completed, None);
        let record = results
            .finish("trash".into(), 0, &Ok("Trashed".into()))
            .unwrap();
        let reopened: Record =
            serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(reopened.items[0].descendants_enumerated);
        assert_eq!(reopened.items[0].outcome, Outcome::Completed);
        assert_eq!(reopened.items[1].parent, Some(0));
        assert_eq!(reopened.items[1].outcome, Outcome::PublicationUnconfirmed);
        assert!(reopened.items[1].reason.is_some());
    }

    #[test]
    fn skipped_selection_limits_do_not_change_skip_or_claim_tree_coverage() {
        let root = local("/tree");
        let mut results = Results::default();
        results.plan([(root.clone(), Some("/target".into()))]);
        results.begin(&root);
        results.weight = MAX_RECORD_WEIGHT;
        let mut covered = vec![
            root.join("not-retained").path().to_owned(),
            root.join("another").path().to_owned(),
            root.join("not-retained").path().to_owned(),
            root.join("another").path().to_owned(),
        ];
        results.skip_selected(&root, "/target".into(), &mut covered);
        let record = results
            .finish("copy".into(), 0, &Ok("Skipped".into()))
            .unwrap();
        assert_eq!(record.items.len(), 1);
        assert_eq!(
            record.omitted, 2,
            "each distinct omitted selection counts once"
        );
        assert!(!record.items[0].descendants_enumerated);
        assert_eq!(record.items[0].outcome, Outcome::Skipped);
    }

    #[test]
    fn new_plan_resets_previous_cancellation_and_partial_removal() {
        let root = local("/tree");
        let mut results = Results::default();
        results.plan([(root.clone(), None)]);
        results.begin(&root);
        results.removing(&root);
        results.removing(&root.join("visited"));
        results.cancelled = true;
        results.plan([(root.clone(), None)]);
        results.begin(&root);
        let record = results
            .finish("move".into(), 0, &Ok("moved".into()))
            .unwrap();
        assert!(!record.cancelled);
        assert_eq!(record.items.len(), 1);
        assert!(!record.items[0].descendants_enumerated);
        assert_eq!(record.items[0].outcome, Outcome::Completed);
    }

    #[test]
    fn history_acknowledgment_reports_save_failure_and_subsequent_recovery() {
        assert!(HistorySender::default().append(record()).is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let ctx = egui::Context::default();
        let mut history = History::open(path.clone(), &ctx);
        assert!(history.sender().append(record()).is_ok());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let existing = path.join("existing");
        fs::write(&existing, b"preserve the destination").unwrap();
        let error = history.sender().append(record()).unwrap_err();
        assert!(matches!(
            &*error,
            HistoryError::Save { path: failed, problem: HistorySaveProblem::Replace(source) }
                if failed == &path && source.raw_os_error().is_some()
        ));
        assert_eq!(fs::read(&existing).unwrap(), b"preserve the destination");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        history.poll();
        assert!(history.error.is_some());
        assert_eq!(
            history.records.len(),
            2,
            "failed save stays reviewable in memory"
        );
        fs::remove_file(existing).unwrap();
        fs::remove_dir(&path).unwrap();
        assert!(history.sender().append(record()).is_ok());
        history.poll();
        assert!(history.error.is_none());
        let saved: Saved = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(
            saved.records.len(),
            3,
            "successful retry persists earlier in-memory results"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn history_worker_preserves_newer_results_after_losing_its_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let lock = path.with_extension("lock");
        let retired = dir.path().join("retired.lock");
        let ctx = egui::Context::default();
        let mut original = History::open(path.clone(), &ctx);
        original.sender().append(record()).unwrap();

        fs::rename(&lock, &retired).unwrap();
        let replacement = History::open(path.clone(), &ctx);
        let mut newer = record();
        newer.message = "committed by the other writer".into();
        replacement.sender().append(newer).unwrap();
        let committed = fs::read(&path).unwrap();
        let contents: Saved = serde_json::from_slice(&committed).unwrap();
        assert_eq!(contents.records[1].message, "committed by the other writer");

        let error = original.sender().append(record()).unwrap_err();
        assert!(matches!(
            &*error,
            HistoryError::Save { path: changed, problem: HistorySaveProblem::LockChanged }
                if changed == &lock
        ));
        assert_eq!(fs::read(&path).unwrap(), committed);

        drop(replacement);
        fs::rename(&retired, &lock).unwrap();
        let retry = original.sender().append(record()).unwrap_err();
        assert!(Arc::ptr_eq(&error, &retry));
        original.poll();
        assert_eq!(original.records.len(), 3);
        assert_eq!(fs::read(path).unwrap(), committed);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn old_v1_history_decodes_with_selected_item_scope() {
        let mut value = serde_json::to_value(Saved {
            version: VERSION,
            records: vec![record()],
        })
        .unwrap();
        let record = value["records"][0].as_object_mut().unwrap();
        record.remove("omitted");
        record.remove("details_truncated");
        for row in record["items"].as_array_mut().unwrap() {
            let row = row.as_object_mut().unwrap();
            row.remove("parent");
            row.remove("descendants_enumerated");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let (reopened, _lock) = open_store(&path).unwrap();
        assert_eq!(reopened.records[0].omitted, 0);
        assert!(!reopened.records[0].details_truncated);
        assert!(!reopened.records[0].items[0].descendants_enumerated);
        assert!(reopened.records[0].items[0].parent.is_none());
    }
}
