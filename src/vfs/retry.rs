//! Session-owned retry evidence. Display paths in saved history are never executable input.
use super::transfer::{
    canonical, entry_metadata, remote_source_matches, EntryMetadata, TransferItem,
    TransferSourceManifest,
};
use super::Location;
use crate::file_error::{AccessAction, AccessError, RetryProblem};
use crate::fs_ops::TransferKind;
use crate::operations::{Cancellation, TransferError};
use crate::result_history::{Outcome, Results};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

mod inspection;

const MAX_RETRY_ROOTS: usize = 100_000;

#[derive(Clone)]
struct Source {
    original: Location,
    resolved: Location,
    metadata: EntryMetadata,
    attempt: Option<Attempt>,
    review: Option<Outcome>,
    replay_allowed: bool,
}

#[derive(Clone)]
struct Attempt {
    target: Location,
    previous: Option<EntryMetadata>,
    staging: Option<Location>,
    held_source: Option<Location>,
    copied: bool,
    source_entries: Option<Arc<TransferSourceManifest>>,
    require_payload: bool,
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

    pub fn weight(&self) -> usize {
        self.sources.iter().fold(self.len(), |weight, source| {
            weight.saturating_add(
                source
                    .attempt
                    .as_ref()
                    .and_then(|attempt| attempt.source_entries.as_ref())
                    .map_or(0, |manifest| manifest.entries.len()),
            )
        })
    }

    pub fn needs_inspection(&self) -> bool {
        self.sources.iter().any(|source| source.review.is_some())
    }

    pub(super) fn checking(&self) -> bool {
        self.checking
    }

    pub(super) fn verify_request(
        &self,
        sources: &[Location],
        destination: &Location,
        kind: TransferKind,
    ) -> Result<(), TransferError> {
        if !self.checking {
            return Ok(());
        }
        if self.needs_inspection() {
            return Err(RetryProblem::InspectionRequired.at(&self.destination));
        }
        if destination != &self.destination {
            return Err(RetryProblem::DestinationChanged.at(destination));
        }
        if kind != self.kind
            || sources.len() != self.sources.len()
            || sources
                .iter()
                .any(|source| !self.indices.contains_key(source))
        {
            return Err(RetryProblem::RequestChanged.at(destination));
        }
        self.verify_target()
    }

    pub(super) fn prepare(
        &mut self,
        plan: &[TransferItem],
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        if self.checking {
            if plan.len() != self.sources.len() {
                return Err(RetryProblem::RequestChanged.at(&self.destination));
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
                        attempt: None,
                        review: None,
                        replay_allowed: true,
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
        self.verify_snapshot(item, actual.as_ref())?;
        if let Some(attempt) = self
            .source(&item.original)
            .and_then(|source| source.attempt.as_ref())
        {
            attempt.verify()?;
            if let Some(manifest) = &attempt.source_entries {
                manifest.verify(cancel, |_, _| {})?;
            }
        }
        Ok(())
    }

    pub(super) fn verify_snapshot(
        &self,
        item: &TransferItem,
        actual: Option<&EntryMetadata>,
    ) -> Result<(), TransferError> {
        if !self.checking {
            return Ok(());
        }
        let changed = || RetryProblem::SourceChanged.at(&item.original);
        let expected = self
            .source(&item.original)
            .filter(|expected| expected.resolved == item.source)
            .ok_or_else(changed)?;
        if expected.matches(actual)? {
            Ok(())
        } else {
            Err(changed())
        }
    }

    fn source(&self, original: &Location) -> Option<&Source> {
        self.indices
            .get(original)
            .map(|&index| &self.sources[index])
    }

    pub(super) fn use_previous_target(&self, item: &mut TransferItem) {
        if let Some(attempt) = self
            .source(&item.original)
            .and_then(|source| source.attempt.as_ref())
        {
            item.target = attempt.target.clone();
        }
    }

    pub(super) fn begin_attempt(&mut self, item: &TransferItem, copied: bool) {
        if (item.source.remote() || item.target.remote())
            && let Some(&index) = self.indices.get(&item.original)
        {
            self.sources[index].attempt = Some(Attempt {
                target: item.target.clone(),
                previous: item.replacement.as_ref().map(|(_, target)| target.clone()),
                staging: None,
                held_source: None,
                copied,
                source_entries: None,
                require_payload: false,
            });
        }
    }

    fn attempt_mut(&mut self, original: &Location) -> Option<&mut Attempt> {
        let index = *self.indices.get(original)?;
        self.sources[index].attempt.as_mut()
    }

    pub(super) fn note_staging(&mut self, original: &Location, staging: &Location) {
        if let Some(attempt) = self.attempt_mut(original) {
            attempt.staging = Some(staging.clone());
        }
    }

    pub(super) fn note_held_source(&mut self, original: &Location, held: &Location) {
        if let Some(attempt) = self.attempt_mut(original) {
            attempt.held_source = Some(held.clone());
        }
    }

    pub(super) fn note_source_manifest(
        &mut self,
        original: &Location,
        manifest: Option<TransferSourceManifest>,
    ) {
        if let Some(attempt) = self.attempt_mut(original) {
            attempt.source_entries = manifest
                .filter(|manifest| manifest.entries.len() <= MAX_RETRY_ROOTS)
                .map(Arc::new);
        }
    }

    fn target_matches(&self) -> Result<bool, TransferError> {
        let actual = entry_metadata(&self.destination)?;
        let (Some((path, expected)), Some(actual)) = (&self.target, actual) else {
            return Ok(false);
        };
        if canonical(&self.destination)? != *path {
            return Ok(false);
        }
        Ok(match (expected, &actual) {
            (EntryMetadata::Local(_), EntryMetadata::Local(after)) => {
                after.is_dir() && expected.same_object(&actual)
            }
            (EntryMetadata::Remote(_), EntryMetadata::Remote(after)) => after.is_dir(),
            _ => false,
        })
    }

    fn verify_target(&self) -> Result<(), TransferError> {
        if self.target_matches()? {
            Ok(())
        } else {
            Err(RetryProblem::DestinationChanged.at(&self.destination))
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
                        | Outcome::PartialCopyRetained
                )
            })
            .map(|(path, _)| path.clone())
            .collect();
        self.sources.retain_mut(|source| {
            let observed = outcomes.get(&source.original).copied();
            let needs_review = source.attempt.is_some()
                && matches!(
                    observed,
                    Some(Outcome::PublicationUnconfirmed | Outcome::CopiedSourceRemaining)
                );
            let eligible = outcomes.get(&source.original).is_some_and(|outcome| {
                matches!(
                    outcome,
                    Outcome::Failed | Outcome::Cancelled | Outcome::NotStarted | Outcome::Skipped
                )
            });
            let has_blocked_descendant = blocked
                .range((
                    std::ops::Bound::Excluded(source.original.clone()),
                    std::ops::Bound::Unbounded,
                ))
                .next()
                .is_some_and(|path| {
                    path.same_backend(&source.original)
                        && path.path().starts_with(source.original.path())
                });
            let mut ancestor = source.original.parent();
            let mut has_blocked_ancestor = false;
            while let Some(path) = ancestor {
                if blocked.contains(&path) {
                    has_blocked_ancestor = true;
                    break;
                }
                ancestor = path.parent().filter(|parent| parent != &path);
            }
            source.replay_allowed = !has_blocked_descendant && !has_blocked_ancestor;
            source.review = observed.filter(|_| needs_review);
            needs_review
                || (eligible && !blocked.contains(&source.original) && source.replay_allowed)
        });
        self.reindex();
        self.checking = true;
        (!self.sources.is_empty() && (self.target.is_some() || self.needs_inspection()))
            .then_some(self)
    }
}

impl Source {
    fn matches(&self, actual: Option<&EntryMetadata>) -> Result<bool, TransferError> {
        match (&self.metadata, actual) {
            (EntryMetadata::Local(before), Some(actual @ EntryMetadata::Local(after)))
                if self.metadata.same_object(actual) =>
            {
                crate::fs_ops::source_metadata_matches(before, after).map_err(|source| {
                    AccessError::new(&self.resolved, AccessAction::ReadMetadata, source).into()
                })
            }
            (EntryMetadata::Remote(before), Some(EntryMetadata::Remote(after))) => {
                Ok(remote_source_matches(before, after))
            }
            _ => Ok(false),
        }
    }
}

impl Attempt {
    fn target_matches(&self, actual: Option<&EntryMetadata>) -> bool {
        match (&self.previous, actual) {
            (None, None) => true,
            (Some(before), Some(now)) => before.matches_file(now),
            _ => false,
        }
    }

    fn backup(&self) -> Option<Location> {
        self.staging
            .as_ref()
            .filter(|_| self.previous.is_some())
            .map(|stage| stage.join("previous"))
    }

    fn payload(&self) -> Option<Location> {
        self.staging
            .as_ref()
            .filter(|_| self.copied)
            .map(|stage| stage.join("payload"))
    }

    fn verify(&self) -> Result<(), TransferError> {
        if !self.target_matches(entry_metadata(&self.target)?.as_ref()) {
            return Err(RetryProblem::TargetChanged.at(&self.target));
        }
        if let Some(backup) = self.backup()
            && entry_metadata(&backup)?.is_some()
        {
            return Err(RetryProblem::BackupRetained.at(&backup));
        }
        if let Some(original) = &self.held_source
            && entry_metadata(original)?.is_some()
        {
            return Err(RetryProblem::OriginalRetained.at(original));
        }
        if self.require_payload {
            let payload = self
                .payload()
                .ok_or_else(|| RetryProblem::PayloadPathMissing.at(&self.target))?;
            if entry_metadata(&payload)?.is_none() {
                return Err(RetryProblem::PayloadMissing.at(&payload));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_error::Failure;
    use std::fs;

    #[test]
    fn retry_reports_the_exact_path_of_each_retained_or_missing_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let target: Location = dir.path().join("destination").into();
        let held: Location = dir.path().join("held-original").into();
        let stage: Location = dir.path().join("staging").into();
        let backup = stage.join("previous");
        let payload = stage.join("payload");
        fs::create_dir(stage.path()).unwrap();
        fs::write(target.path(), b"original destination").unwrap();
        fs::write(held.path(), b"held source").unwrap();
        fs::write(backup.path(), b"saved backup").unwrap();
        let mut attempt = Attempt {
            previous: entry_metadata(&target).unwrap(),
            target: target.clone(),
            staging: Some(stage),
            held_source: Some(held.clone()),
            copied: true,
            source_entries: None,
            require_payload: true,
        };
        for (path, problem, original_bytes) in [
            (
                &backup,
                RetryProblem::BackupRetained,
                b"saved backup".as_slice(),
            ),
            (
                &held,
                RetryProblem::OriginalRetained,
                b"held source".as_slice(),
            ),
        ] {
            let error = attempt.verify().unwrap_err();
            assert!(
                matches!(error, TransferError::Failed(Failure::Retry(reason))
                if reason.problem == problem && &reason.location == path)
            );
            assert_eq!(fs::read(path.path()).unwrap(), original_bytes);
            fs::remove_file(path.path()).unwrap();
        }
        let error = attempt.verify().unwrap_err();
        assert!(
            matches!(error, TransferError::Failed(Failure::Retry(reason))
            if reason.problem == RetryProblem::PayloadMissing && reason.location == payload)
        );
        fs::write(payload.path(), b"temporary copy").unwrap();
        assert!(attempt.verify().is_ok());
        attempt.staging = None;
        let error = attempt.verify().unwrap_err();
        assert!(
            matches!(error, TransferError::Failed(Failure::Retry(reason))
            if reason.problem == RetryProblem::PayloadPathMissing && reason.location == target)
        );
        assert_eq!(fs::read(target.path()).unwrap(), b"original destination");
        assert_eq!(fs::read(payload.path()).unwrap(), b"temporary copy");
    }
}
