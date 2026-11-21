//! In-memory evidence for reversing confirmed renames. Saved display paths are never replayed.
use super::transfer::{
    canonical, entry_metadata, rename_for_publication, source_children, EntryMetadata,
    TransferSourceEntry, TransferSourceManifest,
};
use super::Location;
use crate::operations::{Cancellation, Progress, TransferError};
use crate::result_history::Outcome;
use std::collections::BTreeMap;

pub(super) const MAX_UNDO_ENTRIES: usize = 100_000;

#[derive(Clone)]
pub(super) struct Parent {
    pub(super) location: Location,
    metadata: EntryMetadata,
}

impl Parent {
    pub(super) fn capture(location: &Location) -> Result<Self, String> {
        let location = location.with_path(canonical(location)?);
        let metadata = entry_metadata(&location)?.ok_or_else(unavailable)?;
        let parent = Self { location, metadata };
        parent.verify()?;
        Ok(parent)
    }

    pub(super) fn verify(&self) -> Result<(), String> {
        let actual = entry_metadata(&self.location)?.ok_or_else(unavailable)?;
        let same = match (&self.metadata, &actual) {
            (EntryMetadata::Local(_), EntryMetadata::Local(actual)) => {
                actual.is_dir()
                    && self
                        .metadata
                        .same_object(&EntryMetadata::Local(actual.clone()))
            }
            // SFTP v3 has no stable file ID. A canonical directory and server
            // metadata are the available evidence, not a remote filesystem lock.
            (EntryMetadata::Remote(_), EntryMetadata::Remote(actual)) => actual.is_dir(),
            _ => false,
        };
        if same && canonical(&self.location)? == self.location.path() {
            Ok(())
        } else {
            Err(unavailable())
        }
    }
}

fn unavailable() -> String {
    crate::tr_format!(
        "Undo unavailable: a file or folder changed or is inaccessible. Inspect both paths."
    )
}

#[derive(Clone)]
pub(super) struct Capture {
    original: Location,
    current: Location,
    parents: [Parent; 2],
}

impl Capture {
    pub(super) fn before(source: &Location, target: &Location) -> Result<Self, String> {
        if source == target || !source.same_backend(target) || source.is_archive() {
            return Err(unavailable());
        }
        let parents = [
            Parent::capture(&source.parent().ok_or_else(unavailable)?)?,
            Parent::capture(&target.parent().ok_or_else(unavailable)?)?,
        ];
        let original = parents[0]
            .location
            .join(source.path().file_name().ok_or_else(unavailable)?);
        let current = parents[1]
            .location
            .join(target.path().file_name().ok_or_else(unavailable)?);
        if original == current || entry_metadata(&current)?.is_some() {
            return Err(unavailable());
        }
        Ok(Self {
            original,
            current,
            parents,
        })
    }

    pub(super) fn finish(
        self,
        source: &Location,
        manifest: &TransferSourceManifest,
    ) -> Result<UndoEntry, String> {
        for parent in &self.parents {
            parent.verify()?;
        }
        if entry_metadata(&self.original)?.is_some() {
            return Err(unavailable());
        }
        let mut entries = BTreeMap::new();
        for (path, expected) in &manifest.entries {
            let relative = path
                .path()
                .strip_prefix(source.path())
                .map_err(|_| unavailable())?;
            let current = if relative.as_os_str().is_empty() {
                self.current.clone()
            } else {
                self.current.join(relative)
            };
            let metadata = entry_metadata(&current)?.ok_or_else(unavailable)?;
            if !expected.matches_moved_target(&metadata) {
                return Err(unavailable());
            }
            entries.insert(
                current,
                TransferSourceEntry {
                    metadata,
                    children: expected.children.clone(),
                },
            );
        }
        if entries.is_empty() || entries.len() > MAX_UNDO_ENTRIES {
            return Err(unavailable());
        }
        Ok(UndoEntry {
            capture: self,
            entries,
        })
    }
}

#[derive(Clone)]
pub(super) struct UndoEntry {
    capture: Capture,
    entries: BTreeMap<Location, TransferSourceEntry>,
}

impl UndoEntry {
    fn verify(&self, cancel: &Cancellation) -> Result<(), TransferError> {
        for parent in &self.capture.parents {
            parent.verify()?;
        }
        if entry_metadata(&self.capture.original)?.is_some() {
            return Err(crate::tr_format!(
                "Undo not started: original name {} is occupied. The existing item was preserved.",
                self.capture.original
            )
            .into());
        }
        for (path, expected) in &self.entries {
            cancel.check()?;
            let actual = entry_metadata(path)?.ok_or_else(unavailable)?;
            // Ignore rename-only ctime changes, so two renames can be undone
            // in reverse order. Identity, contents metadata and children must match.
            let unchanged = expected.matches_moved_target(&actual)
                && match (&expected.metadata, &actual) {
                    (EntryMetadata::Local(before), EntryMetadata::Local(after)) => {
                        before.permissions() == after.permissions()
                            && before.created().ok() == after.created().ok()
                    }
                    _ => true,
                };
            if !unchanged {
                return Err(unavailable().into());
            }
            if let Some(names) = &expected.children
                && *names != source_children(path, cancel, true)?
            {
                return Err(unavailable().into());
            }
        }
        for parent in &self.capture.parents {
            parent.verify()?;
        }
        cancel.check()?;
        Ok(())
    }

    fn restore(&self, progress: &mut Progress) -> Result<(), TransferError> {
        let source = &self.capture.current;
        let target = &self.capture.original;
        let cancel = progress.cancellation();
        self.verify(&cancel)?;
        for path in self.entries.keys() {
            progress.results.capturing(source, target, path);
        }
        progress.results.captured();
        progress.item(source.path());
        // No replacement option. The final OS/SFTP rename also refuses an
        // occupied target, including one that appeared after preflight.
        rename_for_publication(source, target)?;
        let mut verified = true;
        for (path, expected) in &self.entries {
            let relative = path
                .path()
                .strip_prefix(source.path())
                .map_err(|_| unavailable())?;
            let restored = if relative.as_os_str().is_empty() {
                target.clone()
            } else {
                target.join(relative)
            };
            let matches = entry_metadata(&restored)
                .ok()
                .flatten()
                .is_some_and(|actual| expected.matches_moved_target(&actual));
            verified &= matches;
            if path != source {
                progress
                    .results
                    .moved_entry(path, matches, (!matches).then(unavailable));
            }
        }
        if verified {
            Ok(())
        } else {
            // Rename was acknowledged, but the final object cannot be proven.
            // Never offer a second inverse action for this receipt.
            Err(TransferError::publication_unconfirmed(unavailable()))
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct UndoBatch {
    entries: Vec<UndoEntry>,
    weight: usize,
}

impl UndoBatch {
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn weight(&self) -> usize {
        self.weight
    }

    pub(super) fn can_fit(&self, entries: usize) -> bool {
        self.weight.saturating_add(entries) <= MAX_UNDO_ENTRIES
    }

    pub(super) fn push(&mut self, entry: UndoEntry) {
        if self.can_fit(entry.entries.len()) {
            self.weight += entry.entries.len();
            self.entries.push(entry);
        }
    }

    pub fn restore(&mut self, progress: &mut Progress) -> Result<usize, TransferError> {
        progress
            .results
            .plan(self.entries.iter().rev().map(|entry| {
                (
                    entry.capture.current.clone(),
                    Some(entry.capture.original.display()),
                )
            }));
        let mut completed = 0;
        while let Some(entry) = self.entries.last() {
            progress.results.begin(&entry.capture.current);
            let result = entry.restore(progress);
            let outcome = match &result {
                Ok(()) => Outcome::Completed,
                Err(TransferError::Cancelled) => {
                    progress.results.cancelled = true;
                    Outcome::Cancelled
                }
                Err(TransferError::PublicationUnconfirmed(_)) => Outcome::PublicationUnconfirmed,
                Err(TransferError::Failed(_)) => Outcome::Failed,
            };
            progress.results.record(
                &entry.capture.current,
                Some(entry.capture.original.display()),
                outcome,
                result.as_ref().err().map(crate::i18n::file_error),
            );
            if result.is_ok() || matches!(result, Err(TransferError::PublicationUnconfirmed(_))) {
                self.weight -= self.entries.pop().unwrap().entries.len();
            }
            result?;
            completed += 1;
        }
        Ok(completed)
    }
}

pub(crate) fn rename_with_undo(
    source: &Location,
    name: &str,
    progress: &mut Progress,
) -> Result<Location, TransferError> {
    super::location::validate_name(name)?;
    let target = source
        .parent()
        .ok_or_else(|| crate::file_error::MutationError::UndoUnavailable(source.clone()))?
        .join(name);
    progress
        .results
        .plan([(source.clone(), Some(target.display()))]);
    progress.results.begin(source);
    progress.item(source.path());
    let evidence = Capture::before(source, &target).ok().and_then(|capture| {
        TransferSourceManifest::capture_move_observed(
            source,
            &progress.cancellation(),
            &mut |_, _| {},
        )
        .ok()
        .map(|manifest| (capture, manifest))
    });
    progress.check_cancelled()?;
    let renamed = if source.remote() && source != &target {
        match rename_for_publication(source, &target) {
            Ok(()) => target.clone(),
            Err(error) => {
                let outcome = if matches!(error, TransferError::PublicationUnconfirmed(_)) {
                    Outcome::PublicationUnconfirmed
                } else {
                    Outcome::Failed
                };
                progress.results.record(
                    source,
                    Some(target.display()),
                    outcome,
                    Some(crate::i18n::file_error(&error)),
                );
                return Err(error);
            }
        }
    } else {
        super::mutations::rename_entry(source, name)?
    };
    progress.results.target(source, renamed.display());
    if let Some((capture, manifest)) = evidence
        && let Ok(entry) = capture.finish(source, &manifest)
    {
        progress.undo.push(entry);
    }
    Ok(renamed)
}
