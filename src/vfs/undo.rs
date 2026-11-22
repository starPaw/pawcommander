//! In-memory evidence for reversing confirmed renames. Saved display paths are never replayed.
use super::transfer::{
    canonical, entry_metadata, rename_for_publication, source_children, EntryMetadata,
    TransferSourceEntry, TransferSourceManifest,
};
use super::Location;
use crate::file_error::{MutationError, UndoError};
use crate::operations::{Cancellation, Progress, TransferError};
use crate::result_history::Outcome;
use std::collections::BTreeMap;
#[cfg(target_os = "macos")]
use {
    super::transfer::LocalEntryMetadata,
    crate::{
        file_error::{AccessAction, AccessError, SmbMutationAction},
        smb::MutationMount,
    },
    std::fs::File,
};

pub(super) const MAX_UNDO_ENTRIES: usize = 100_000;

#[derive(Clone)]
pub(super) struct Parent {
    pub(super) location: Location,
    metadata: EntryMetadata,
}

impl Parent {
    pub(super) fn capture(location: &Location) -> Result<Self, TransferError> {
        let location = location.with_path(canonical(location)?);
        let metadata = entry_metadata(&location)?.ok_or_else(|| unavailable(&location))?;
        let parent = Self { location, metadata };
        parent.verify()?;
        Ok(parent)
    }

    pub(super) fn verify(&self) -> Result<(), TransferError> {
        if self.matches()? {
            Ok(())
        } else {
            Err(unavailable(&self.location).into())
        }
    }

    pub(super) fn matches(&self) -> Result<bool, TransferError> {
        let Some(actual) = entry_metadata(&self.location)? else {
            return Ok(false);
        };
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
        Ok(same && canonical(&self.location)? == self.location.path())
    }

    #[cfg(target_os = "macos")]
    pub(super) fn matches_handle(&self, handle: &File) -> Result<bool, TransferError> {
        let actual = handle.metadata().map_err(|error| {
            AccessError::new(&self.location, AccessAction::ReadOpenMetadata, error)
        })?;
        Ok(actual.is_dir()
            && self
                .metadata
                .same_object(&EntryMetadata::Local(LocalEntryMetadata::from_file(
                    handle, actual,
                ))))
    }
}

fn unavailable(location: &Location) -> MutationError {
    MutationError::UndoUnavailable(location.clone())
}

#[derive(Clone)]
pub(super) struct Capture {
    original: Location,
    current: Location,
    parents: [Parent; 2],
    #[cfg(target_os = "macos")]
    mount: MutationMount,
}

impl Capture {
    pub(super) fn before(
        source: &Location,
        target: &Location,
        #[cfg(target_os = "macos")] forward_mount: Option<&MutationMount>,
    ) -> Result<Self, TransferError> {
        if source == target || !source.same_backend(target) || source.is_archive() {
            return Err(unavailable(source).into());
        }
        #[cfg(target_os = "macos")]
        let mut mount = {
            let mut mount = forward_mount
                .cloned()
                .unwrap_or_else(|| MutationMount::new(SmbMutationAction::Undo));
            for location in [source, target] {
                if let Some(path) = location.local() {
                    mount.check(path)?;
                }
            }
            mount
        };
        let parents = [
            Parent::capture(&source.parent().ok_or_else(|| unavailable(source))?)?,
            Parent::capture(&target.parent().ok_or_else(|| unavailable(target))?)?,
        ];
        let original = parents[0].location.join(
            source
                .path()
                .file_name()
                .ok_or_else(|| unavailable(source))?,
        );
        let current = parents[1].location.join(
            target
                .path()
                .file_name()
                .ok_or_else(|| unavailable(target))?,
        );
        if original == current {
            return Err(unavailable(&current).into());
        }
        #[cfg(target_os = "macos")]
        {
            for (requested, resolved) in [(source, &original), (target, &current)] {
                if requested != resolved
                    && let Some(path) = resolved.local()
                {
                    mount.check(path)?;
                }
            }
            mount.recheck()?;
        }
        if entry_metadata(&current)?.is_some() {
            return Err(MutationError::DestinationExists(current).into());
        }
        Ok(Self {
            original,
            current,
            parents,
            #[cfg(target_os = "macos")]
            mount,
        })
    }

    fn verify_mount(&self) -> Result<(), TransferError> {
        #[cfg(target_os = "macos")]
        self.mount.recheck()?;
        Ok(())
    }

    pub(super) fn finish(
        self,
        source: &Location,
        manifest: &TransferSourceManifest,
    ) -> Result<UndoEntry, TransferError> {
        self.verify_mount()?;
        for parent in &self.parents {
            parent.verify()?;
        }
        if entry_metadata(&self.original)?.is_some() {
            return Err(unavailable(&self.original).into());
        }
        let mut entries = BTreeMap::new();
        for (path, expected) in &manifest.entries {
            let relative = path
                .path()
                .strip_prefix(source.path())
                .map_err(|_| unavailable(path))?;
            let current = if relative.as_os_str().is_empty() {
                self.current.clone()
            } else {
                self.current.join(relative)
            };
            let metadata = entry_metadata(&current)?.ok_or_else(|| unavailable(&current))?;
            if !expected.matches_moved_target(&metadata) {
                return Err(unavailable(&current).into());
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
            return Err(unavailable(&self.current).into());
        }
        self.verify_mount()?;
        #[cfg(target_os = "macos")]
        let capture = Self {
            mount: self.mount.with_action(SmbMutationAction::Undo),
            ..self
        };
        #[cfg(not(target_os = "macos"))]
        let capture = self;
        Ok(UndoEntry { capture, entries })
    }

    fn restore_original_name(&self) -> Result<(), TransferError> {
        let source = &self.current;
        let target = &self.original;
        #[cfg(target_os = "macos")]
        if let Some(source_parent) = self.mount.open_parent(source.path())? {
            let check = |parent: &Parent, handle: &File| -> Result<(), TransferError> {
                if parent.matches_handle(handle)? {
                    Ok(())
                } else {
                    Err(UndoError::FolderChanged(parent.location.clone()).into())
                }
            };
            check(&self.parents[1], &source_parent)?;
            let target_parent = self
                .mount
                .open_parent(target.path())?
                .ok_or_else(|| unavailable(target))?;
            check(&self.parents[0], &target_parent)?;
            return crate::fs_ops::rename_between_open_directories(
                &source_parent,
                &target_parent,
                source.path(),
                target.path(),
            );
        }
        rename_for_publication(source, target)
    }
}

#[derive(Clone)]
pub(super) struct UndoEntry {
    capture: Capture,
    entries: BTreeMap<Location, TransferSourceEntry>,
}

impl UndoEntry {
    fn verify_folders(&self) -> Result<(), TransferError> {
        self.capture.verify_mount()?;
        for parent in &self.capture.parents {
            if !parent.matches()? {
                return Err(UndoError::FolderChanged(parent.location.clone()).into());
            }
        }
        Ok(())
    }

    fn verify(&self, cancel: &Cancellation) -> Result<(), TransferError> {
        self.verify_folders()?;
        if entry_metadata(&self.capture.original)?.is_some() {
            return Err(UndoError::OriginalOccupied {
                target: self.capture.original.clone(),
                source: None,
            }
            .into());
        }
        for (path, expected) in &self.entries {
            cancel.check()?;
            let actual =
                entry_metadata(path)?.ok_or_else(|| UndoError::ItemMissing(path.clone()))?;
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
                return Err(UndoError::ItemChanged(path.clone()).into());
            }
            if let Some(names) = &expected.children
                && *names != source_children(path, cancel, true)?
            {
                return Err(UndoError::ItemChanged(path.clone()).into());
            }
        }
        self.verify_folders()?;
        cancel.check()?;
        Ok(())
    }

    fn restore(
        &self,
        progress: &mut Progress,
        read_restored: &mut impl FnMut(&Location) -> Result<Option<EntryMetadata>, TransferError>,
    ) -> Result<(), TransferError> {
        let source = &self.capture.current;
        let target = &self.capture.original;
        let cancel = progress.cancellation();
        cancel.check()?;
        self.verify(&cancel)?;
        for path in self.entries.keys() {
            progress.results.capturing(source, target, path);
        }
        progress.results.captured();
        progress.item(source.path());
        cancel.check()?;
        // No replacement option. The final OS/SFTP rename also refuses an
        // occupied target, including one that appeared after preflight.
        self.capture.restore_original_name().map_err(|error| {
            error.map_occupied_rename(|source| {
                UndoError::OriginalOccupied {
                    target: target.clone(),
                    source: Some(source),
                }
                .into()
            })
        })?;
        self.verify_restored(progress, read_restored)
            .map_err(|cause| {
                // Every verification error follows an acknowledged rename.
                TransferError::from(UndoError::Unconfirmed {
                    from: source.clone(),
                    to: target.clone(),
                    cause,
                })
                .with_unconfirmed_publication()
            })
    }

    fn verify_restored(
        &self,
        progress: &mut Progress,
        read_restored: &mut impl FnMut(&Location) -> Result<Option<EntryMetadata>, TransferError>,
    ) -> Result<(), TransferError> {
        self.capture.verify_mount()?;
        let source = &self.capture.current;
        let target = &self.capture.original;
        let mut failure = None;
        for (path, expected) in &self.entries {
            let relative = path
                .path()
                .strip_prefix(source.path())
                .map_err(|_| UndoError::RestoredItemMismatch(path.clone()))?;
            let restored = if relative.as_os_str().is_empty() {
                target.clone()
            } else {
                target.join(relative)
            };
            let check = read_restored(&restored).and_then(|actual| {
                if actual.is_some_and(|actual| expected.matches_moved_target(&actual)) {
                    Ok(())
                } else {
                    Err(UndoError::RestoredItemMismatch(restored).into())
                }
            });
            if path != source {
                progress
                    .results
                    .moved_entry_with_error(path, check.as_ref().err());
            }
            if failure.is_none() {
                failure = check.err();
            }
        }
        failure.map_or_else(|| self.capture.verify_mount(), Err)
    }
}

#[derive(Clone, Default)]
pub(crate) struct UndoBatch {
    entries: Vec<UndoEntry>,
    weight: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UndoOutcome {
    Restored(usize),
    Stopped { restored: usize, remaining: usize },
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

    pub fn restore(&mut self, progress: &mut Progress) -> Result<UndoOutcome, TransferError> {
        self.restore_checking(progress, entry_metadata)
    }

    pub(super) fn restore_checking(
        &mut self,
        progress: &mut Progress,
        mut read_restored: impl FnMut(&Location) -> Result<Option<EntryMetadata>, TransferError>,
    ) -> Result<UndoOutcome, TransferError> {
        progress
            .results
            .plan(self.entries.iter().rev().map(|entry| {
                (
                    entry.capture.current.clone(),
                    Some(entry.capture.original.display()),
                )
            }));
        let mut restored = 0;
        while let Some(entry) = self.entries.last() {
            progress.results.begin(&entry.capture.current);
            let result = entry.restore(progress, &mut read_restored);
            let outcome = match &result {
                Ok(()) => Outcome::Completed,
                Err(TransferError::Cancelled) => {
                    progress.results.cancelled = true;
                    Outcome::Cancelled
                }
                Err(TransferError::PublicationUnconfirmed(_)) => {
                    progress.results.unconfirmed_move_descendants();
                    Outcome::PublicationUnconfirmed
                }
                Err(TransferError::Failed(_)) => Outcome::Failed,
            };
            progress.results.record_with_error(
                &entry.capture.current,
                outcome,
                result.as_ref().err(),
            );
            // A root may already have moved after an unconfirmed result.
            if matches!(
                outcome,
                Outcome::Completed | Outcome::PublicationUnconfirmed
            ) {
                self.weight -= self.entries.pop().unwrap().entries.len();
            }
            match result {
                Ok(()) => restored += 1,
                Err(TransferError::Cancelled) => {
                    return Ok(UndoOutcome::Stopped {
                        restored,
                        remaining: self.len(),
                    });
                }
                Err(cause) => {
                    let failure = TransferError::from(UndoError::Incomplete {
                        restored,
                        remaining: self.len(),
                        cause,
                    });
                    return Err(if outcome == Outcome::PublicationUnconfirmed {
                        failure.with_unconfirmed_publication()
                    } else {
                        failure
                    });
                }
            }
        }
        Ok(UndoOutcome::Restored(restored))
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
        .ok_or_else(|| unavailable(source))?
        .join(name);
    progress
        .results
        .plan([(source.clone(), Some(target.display()))]);
    progress.results.begin(source);
    progress.item(source.path());
    #[cfg(target_os = "macos")]
    let local_mount = if let Some(path) = source.local() {
        progress.check_cancelled()?;
        let mut mount = MutationMount::new(SmbMutationAction::Rename);
        mount.check(path)?;
        Some((path, mount))
    } else {
        None
    };
    let capture = Capture::before(
        source,
        &target,
        #[cfg(target_os = "macos")]
        local_mount.as_ref().map(|(_, mount)| mount),
    );
    let evidence = capture.ok().and_then(|capture| {
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
                progress
                    .results
                    .record_with_error(source, outcome, Some(&error));
                return Err(error);
            }
        }
    } else {
        #[cfg(target_os = "macos")]
        let result = match local_mount {
            Some((path, mount)) => {
                crate::fs_ops::rename_entry_checked(path, name, mount).map(Location::Local)
            }
            None => super::mutations::rename_entry(source, name),
        };
        #[cfg(not(target_os = "macos"))]
        let result = super::mutations::rename_entry(source, name);
        result?
    };
    progress.results.target(source, renamed.display());
    if let Some((capture, manifest)) = evidence
        && let Ok(entry) = capture.finish(source, &manifest)
    {
        progress.undo.push(entry);
    }
    Ok(renamed)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn undo_rejects_a_replacement_parent_handle_on_the_same_volume() {
        let fixture = tempfile::tempdir().unwrap();
        let folder = fixture.path().canonicalize().unwrap().join("folder");
        fs::create_dir(&folder).unwrap();
        let saved = Parent::capture(&Location::Local(folder.clone())).unwrap();
        let original = File::open(&folder).unwrap();
        fs::rename(&folder, folder.with_file_name("retained")).unwrap();
        fs::create_dir(&folder).unwrap();

        assert!(saved.matches_handle(&original).unwrap());
        assert!(!saved.matches_handle(&File::open(&folder).unwrap()).unwrap());
    }
}
