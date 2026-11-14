//! Session-owned retry evidence. Display paths in saved history are never executable input.
use super::transfer::{
    canonical, entry_metadata, remote_source_matches, EntryMetadata, TransferItem,
};
use super::Location;
use crate::fs_ops::TransferKind;
use crate::operations::{Cancellation, TransferError};
use crate::result_history::{Outcome, Results};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

const MAX_RETRY_ROOTS: usize = 100_000;

#[derive(Clone)]
struct Source {
    original: Location,
    resolved: Location,
    metadata: EntryMetadata,
}

#[derive(Clone)]
pub(crate) struct RetryTransfer {
    pub kind: TransferKind,
    pub destination: Location,
    sources: Vec<Source>,
    indices: HashMap<Location, usize>,
    target: Option<(PathBuf, EntryMetadata)>,
    checking: bool,
}

impl RetryTransfer {
    pub fn new(kind: TransferKind, destination: Location) -> Self {
        Self {
            kind,
            destination,
            sources: Vec::new(),
            indices: HashMap::new(),
            target: None,
            checking: false,
        }
    }

    pub fn sources(&self) -> Vec<Location> {
        self.sources
            .iter()
            .map(|source| source.original.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub(super) fn checking(&self) -> bool {
        self.checking
    }

    pub(super) fn prepare(
        &mut self,
        plan: &[TransferItem],
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        if self.checking {
            self.verify_target()?;
            if plan.len() != self.sources.len() {
                return Err(crate::tr_format!(
                    "Состав источников изменился. Выберите файлы заново.",
                    "The source selection changed. Select the files again."
                )
                .into());
            }
            for item in plan {
                self.verify(item, cancel)?;
            }
        } else if plan.len() <= MAX_RETRY_ROOTS {
            self.target = canonical(&self.destination)
                .ok()
                .zip(entry_metadata(&self.destination).ok().flatten());
            // Optional evidence cannot turn a normal operation into a failure.
            // Missing/unreadable evidence makes that root unavailable for retry.
            for item in plan {
                cancel.check()?;
                if let Ok(Some(metadata)) = entry_metadata(&item.source) {
                    self.sources.push(Source {
                        original: item.original.clone(),
                        resolved: item.source.clone(),
                        metadata,
                    });
                }
            }
            self.reindex();
        }
        Ok(())
    }

    pub(super) fn verify(
        &self,
        item: &TransferItem,
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        if !self.checking {
            return Ok(());
        }
        cancel.check()?;
        self.verify_target()?;
        let actual = entry_metadata(&item.source)?;
        self.verify_snapshot(item, actual.as_ref())
    }

    pub(super) fn verify_snapshot(
        &self,
        item: &TransferItem,
        actual: Option<&EntryMetadata>,
    ) -> Result<(), TransferError> {
        if !self.checking {
            return Ok(());
        }
        let expected = self
            .indices
            .get(&item.original)
            .map(|&index| &self.sources[index]);
        let unchanged = match (expected, actual) {
            (Some(expected), Some(actual)) if expected.resolved == item.source => {
                match (&expected.metadata, actual) {
                    (EntryMetadata::Local(before), EntryMetadata::Local(after)) => {
                        expected.metadata.same_object(actual)
                            && crate::fs_ops::source_metadata_matches(before, after)
                                .unwrap_or(false)
                    }
                    (EntryMetadata::Remote(before), EntryMetadata::Remote(after)) => {
                        remote_source_matches(before, after)
                    }
                    _ => false,
                }
            }
            _ => false,
        };
        if unchanged {
            Ok(())
        } else {
            Err(crate::tr_format!(
                "Повтор не начат: источник {} изменился или недоступен. Проверьте его и выберите файлы заново.",
                "Retry not started: source {} changed or is unavailable. Inspect it and select the files again.", item.original
            ).into())
        }
    }

    fn verify_target(&self) -> Result<(), TransferError> {
        let actual = entry_metadata(&self.destination)?;
        let unchanged = match (&self.target, actual) {
            (Some((path, expected)), Some(actual))
                if canonical(&self.destination).ok().as_ref() == Some(path) =>
            {
                match (expected, &actual) {
                    (EntryMetadata::Local(_), EntryMetadata::Local(after)) => {
                        after.is_dir() && expected.same_object(&actual)
                    }
                    (EntryMetadata::Remote(_), EntryMetadata::Remote(after)) => after.is_dir(),
                    _ => false,
                }
            }
            _ => false,
        };
        if unchanged {
            Ok(())
        } else {
            Err(crate::tr_format!(
                "Папка назначения изменилась или недоступна. Выберите её заново.",
                "The destination folder changed or is unavailable. Select it again."
            )
            .into())
        }
    }

    fn reindex(&mut self) {
        self.indices = self
            .sources
            .iter()
            .enumerate()
            .map(|(index, source)| (source.original.clone(), index))
            .collect();
    }

    pub fn unfinished(mut self, results: &Results) -> Option<Self> {
        let outcomes: HashMap<_, _> = results.root_outcomes().into_iter().collect();
        let blocked: BTreeSet<_> = outcomes
            .iter()
            .filter(|(_, outcome)| {
                matches!(
                    outcome,
                    Outcome::Completed
                        | Outcome::CompletedWithWarning
                        | Outcome::CopiedSourceRemaining
                        | Outcome::PublicationUnconfirmed
                )
            })
            .map(|(path, _)| path.clone())
            .collect();
        self.sources.retain(|source| {
            let eligible = outcomes.get(&source.original).is_some_and(|outcome| {
                matches!(
                    outcome,
                    Outcome::Failed | Outcome::Cancelled | Outcome::NotStarted | Outcome::Skipped
                )
            });
            let has_blocked_descendant = blocked
                .range(source.original.clone()..)
                .next()
                .is_some_and(|path| {
                    path.same_backend(&source.original)
                        && path.path().starts_with(source.original.path())
                });
            let mut ancestor = Some(source.original.clone());
            let mut has_blocked_ancestor = false;
            while let Some(path) = ancestor {
                if blocked.contains(&path) {
                    has_blocked_ancestor = true;
                    break;
                }
                ancestor = path.parent().filter(|parent| parent != &path);
            }
            eligible && !has_blocked_descendant && !has_blocked_ancestor
        });
        self.reindex();
        self.checking = true;
        (!self.sources.is_empty() && self.target.is_some()).then_some(self)
    }
}
