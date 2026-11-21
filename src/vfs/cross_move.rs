//! Verified copy followed by guarded, nonrecursive source removal.
//! A published copy and a completed move are separate facts. Partial moves
//! are never automatic retry or inverse-rename candidates.
use super::copy_metadata::{self, Attributes, Mode as MetadataMode};
use super::sftp::remote;
use super::transfer::{
    copy_item_cancellable, entry_metadata, rename_for_publication, source_children,
    with_preserved_staging, EntryMetadata, SourceReader, TransferItem, TransferSourceEntry,
    TransferSourceManifest,
};
use super::{err, Location};
use crate::operations::{Cancellation, CopyReader, Progress, TransferError};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;

#[cfg(test)]
mod tests;

pub(super) fn required(item: &TransferItem) -> Result<bool, TransferError> {
    #[cfg(target_os = "macos")]
    if super::transfer::mounted_smb(&item.source) || super::transfer::mounted_smb(&item.target) {
        return Err(
            crate::i18n::text!("Для передачи файлов через SMB используйте копирование.").into(),
        );
    }
    if !item.source.same_backend(&item.target) {
        return Ok(true);
    }
    if item.source.local().is_some() {
        let source = metadata(&item.source)?;
        let parent = item.target.parent().ok_or("Invalid destination")?;
        let target = metadata(&parent)?;
        // If volume identity is unavailable, retain the native rename path;
        // never guess that an arbitrary rename failure permits copy/delete.
        return Ok(source.same_local_volume(&target) == Some(false));
    }
    Ok(false)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Copied,
    #[cfg(target_os = "macos")]
    QuarantinePlanned,
    Quarantined,
    BeforeRemove,
    Removed,
}

fn changed(path: &Location) -> String {
    crate::tr_format!(
        "Move stopped: item {path} changed or is inaccessible",
        path = path
    )
}

fn metadata(path: &Location) -> Result<EntryMetadata, TransferError> {
    entry_metadata(path)?.ok_or_else(|| changed(path).into())
}

fn directory_matches(expected: &EntryMetadata, actual: &EntryMetadata) -> bool {
    match (expected, actual) {
        (EntryMetadata::Local(before), EntryMetadata::Local(after)) => {
            before.is_dir()
                && after.is_dir()
                && expected.same_object(actual)
                && before.permissions() == after.permissions()
        }
        (EntryMetadata::Remote(before), EntryMetadata::Remote(after)) => {
            before.is_dir()
                && after.is_dir()
                && before.perm == after.perm
                && before.uid == after.uid
                && before.gid == after.gid
        }
        _ => false,
    }
}

fn mapped(path: &Location, from: &Location, to: &Location) -> Location {
    let relative = path
        .path()
        .strip_prefix(from.path())
        .expect("manifest root");
    if relative.as_os_str().is_empty() {
        to.clone()
    } else {
        to.join(relative)
    }
}

fn verify_entry(
    path: &Location,
    expected: &TransferSourceEntry,
    cancel: &Cancellation,
) -> Result<(), TransferError> {
    cancel.check()?;
    let actual = metadata(path)?;
    let matches = expected.matches_moved_target(&actual)
        && match (&expected.metadata, &actual) {
            (EntryMetadata::Local(before), EntryMetadata::Local(after)) => {
                before.permissions() == after.permissions()
                    && before.created().ok() == after.created().ok()
            }
            _ => true,
        };
    if !matches {
        return Err(changed(path).into());
    }
    Ok(())
}

fn reader(path: &Location) -> Result<SourceReader, TransferError> {
    Ok(if let Some(local) = path.local() {
        SourceReader::Local(crate::fs_ops::open_regular_source(local).map_err(err)?)
    } else {
        SourceReader::Remote(remote(path)?.open(path.path()).map_err(err)?)
    })
}

fn verify_file_pair(
    source: &Location,
    target: &Location,
    displayed_source: &Location,
    before: &TransferSourceEntry,
    copied: &TransferSourceEntry,
    attributes: Option<Attributes>,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let cancel = progress.cancellation();
    verify_entry(source, before, &cancel)?;
    verify_entry(target, copied, &cancel)?;
    // The original root's ctime changed during quarantine rename. Capture a
    // fresh version now, so later writes that restore size/mtime still stop
    // removal on filesystems that expose a change timestamp.
    let current_source = TransferSourceEntry {
        metadata: metadata(source)?,
        children: None,
    };
    if !before.metadata.is_file() || !copied.metadata.is_file() || before.size() != copied.size() {
        return Err(changed(target).into());
    }
    let mut input = reader(source)?;
    if !before.matches_moved_target(&input.metadata()?) {
        return Err(changed(source).into());
    }
    let mut hashed = CopyReader::new((&mut input).take(before.size()), true);
    let mut buffer = [0; 64 * 1024];
    let mut size = 0;
    loop {
        cancel.check()?;
        let n = hashed.read(&mut buffer).map_err(err)?;
        if n == 0 {
            break;
        }
        size += n as u64;
    }
    let digest = hashed.finish().expect("mandatory move verification");
    if size != before.size() || !before.matches_moved_target(&input.metadata()?) {
        return Err(changed(source).into());
    }
    let mut output = reader(target)?;
    if !copied.matches_moved_target(&output.metadata()?) {
        return Err(changed(target).into());
    }
    progress
        .verify_copy_before_source_removal(displayed_source.path(), size, digest, &mut output)
        .map_err(|error| TransferError::io(error, err))?;
    if !copied.matches_moved_target(&output.metadata()?) {
        return Err(changed(target).into());
    }
    verify_entry(target, copied, &cancel)?;
    copied.verify(target, &cancel, true)?;
    if let Some(wanted) = attributes {
        let warning = copy_metadata::finish_file(target, &wanted, &mut output)?;
        let open_version = TransferSourceEntry {
            metadata: output.metadata()?,
            children: None,
        };
        open_version.verify(target, &cancel, true)?;
        progress
            .results
            .note_metadata(displayed_source, warning.as_deref());
    }
    current_source.verify_metadata(source, &input.metadata()?)?;
    current_source.verify(source, &cancel, true)?;
    verify_entry(source, before, &cancel)
}

fn verify_ancestors(
    original: &Location,
    manifest: &TransferSourceManifest,
    root: &Location,
    current_root: &Location,
    cancel: &Cancellation,
) -> Result<(), TransferError> {
    let mut parent = original.parent();
    while let Some(path) = parent {
        let Some(entry) = manifest.entries.get(&path) else {
            break;
        };
        cancel.check()?;
        let current = mapped(&path, root, current_root);
        if !directory_matches(&entry.metadata, &metadata(&current)?) {
            return Err(changed(&current).into());
        }
        parent = path.parent();
    }
    Ok(())
}

pub(super) fn run(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    progress: &mut Progress,
    published: impl FnOnce(),
    removed: impl FnOnce(),
    observe: &mut impl FnMut(&Location, Phase) -> Result<(), TransferError>,
) -> Result<(), TransferError> {
    #[cfg(target_os = "macos")]
    if let Some(directory) = progress.resume_directory.clone()
        && entry_metadata(&item.source)?.is_some_and(|meta| meta.is_file() || meta.is_directory())
    {
        return super::resume_copy::move_item(
            item, expected, &directory, progress, published, removed, observe,
        );
    }
    let source = MoveSource::capture(item, progress)?;
    let configured = progress.copy_verification();
    progress.set_copy_verification(true);
    let mut copied = false;
    let result = copy_item_cancellable(
        item,
        expected,
        progress,
        MetadataMode::KeepReadableForMove,
        || {
            copied = true;
            published();
        },
    );
    progress.set_copy_verification(configured);
    if !copied {
        return result;
    }
    if let Err(error) = result {
        progress.results.copied_source_remaining();
        return Err(incomplete(&item.target, error, progress));
    }
    remove_verified(item, source, None, progress, removed, observe)
}

pub(super) struct MoveSource {
    manifest: TransferSourceManifest,
    links: BTreeMap<Location, PathBuf>,
    #[cfg(target_os = "macos")]
    original_access_times: Option<Vec<Option<std::time::SystemTime>>>,
}

impl MoveSource {
    pub(super) fn capture(item: &TransferItem, progress: &Progress) -> Result<Self, TransferError> {
        let scan = progress.cancellation().pausable_scan();
        let manifest =
            TransferSourceManifest::capture_move_observed(&item.source, &scan, &mut |_, _| {})?;
        Self::from_manifest(item, manifest)
    }

    pub(super) fn from_manifest(
        item: &TransferItem,
        manifest: TransferSourceManifest,
    ) -> Result<Self, TransferError> {
        let mut links = BTreeMap::new();
        for (path, entry) in &manifest.entries {
            if entry.children.is_none() && !entry.metadata.is_file() {
                // A local symbolic link has a literal target to compare.
                if let (Some(local), Some(_), EntryMetadata::Local(meta)) =
                    (path.local(), item.target.local(), &entry.metadata)
                    && meta.is_symlink()
                    && entry.metadata.same_object(&entry.metadata)
                {
                    links.insert(path.clone(), fs::read_link(local).map_err(err)?);
                } else {
                    return Err(crate::tr_format!(
                        "Safe cross-volume move is unavailable for this object type: {path}",
                        path = path
                    )
                    .into());
                }
            }
        }
        Ok(Self {
            manifest,
            links,
            #[cfg(target_os = "macos")]
            original_access_times: None,
        })
    }

    #[cfg(target_os = "macos")]
    pub(super) fn with_original_access_times(
        mut self,
        times: Option<Vec<Option<std::time::SystemTime>>>,
    ) -> Result<Self, TransferError> {
        let entries = self.manifest.entries.len();
        let times = times.unwrap_or_else(|| vec![None; entries]);
        if times.len() != entries {
            return Err(
                crate::i18n::text!("Некорректная запись времени доступа для переноса").into(),
            );
        }
        // The saved timestamps use the same sorted relative paths as the verified manifest.
        self.original_access_times = Some(times);
        Ok(self)
    }

    fn attributes(&self, metadata: &EntryMetadata, _index: usize) -> Attributes {
        let wanted = Attributes::from_entry(metadata);
        #[cfg(target_os = "macos")]
        if let Some(times) = &self.original_access_times {
            return wanted.with_accessed(times[_index]);
        }
        wanted
    }

    #[cfg(target_os = "macos")]
    pub(super) fn report(&self, item: &TransferItem, progress: &mut Progress) {
        progress.results.descendants(
            &item.source,
            &item.target,
            self.manifest.entries.keys().cloned(),
        );
    }
}

pub(super) fn remove_verified(
    item: &TransferItem,
    source_plan: MoveSource,
    verified_target: Option<TransferSourceManifest>,
    progress: &mut Progress,
    removed: impl FnOnce(),
    observe: &mut impl FnMut(&Location, Phase) -> Result<(), TransferError>,
) -> Result<(), TransferError> {
    remove_verified_at(
        item,
        source_plan,
        verified_target,
        None,
        progress,
        removed,
        observe,
    )
}

pub(super) fn remove_verified_at(
    item: &TransferItem,
    source_plan: MoveSource,
    verified_target: Option<TransferSourceManifest>,
    target_paths: Option<&BTreeMap<Location, Location>>,
    progress: &mut Progress,
    removed: impl FnOnce(),
    observe: &mut impl FnMut(&Location, Phase) -> Result<(), TransferError>,
) -> Result<(), TransferError> {
    let manifest = &source_plan.manifest;
    let links = &source_plan.links;
    let cancel = progress.cancellation();
    let target_path = |original: &Location| {
        if let Some(paths) = target_paths {
            paths
                .get(original)
                .cloned()
                .ok_or_else(|| changed(original))
        } else {
            Ok(mapped(original, &item.source, &item.target))
        }
    };
    progress.results.copied_source_remaining();
    let mut fully_removed = false;
    let result = (|| {
        observe(&item.source, Phase::Copied)?;
        progress.checkpoint()?;
        manifest.verify(&cancel, |_, _| {})?;
        let finish_local_attributes = verified_target.is_some();
        let target_manifest = match verified_target {
            Some(manifest) => {
                manifest.verify(&cancel, |_, _| {})?;
                manifest
            }
            None => TransferSourceManifest::capture_move_observed(
                &item.target,
                &cancel,
                &mut |_, _| {},
            )?,
        };
        with_preserved_staging(&item.source, &cancel, |staging, preserve| {
            let staging_meta = metadata(staging)?;
            let held = staging.join("source");
            if let Some(retry) = &mut progress.retry {
                retry.note_held_source(&item.original, &held);
            }
            #[cfg(target_os = "macos")]
            if let Err(error) = observe(&held, Phase::QuarantinePlanned)
                .and_then(|()| cancel.check().map_err(Into::into))
            {
                *preserve = false;
                return Err(error);
            }
            // This directory is registered for crash recovery and retained
            // even on panic or an unacknowledged remote rename.
            if let Err(error) = rename_for_publication(&item.source, &held) {
                return Err(crate::tr_format!(
                    "{error}. Inspect source {} and retained folder {held}",
                    item.source,
                    error = error,
                    held = held
                )
                .into());
            }
            let deletion = (|| {
                observe(&held, Phase::Quarantined)?;
                // Check the complete tree before deleting its first entry.
                for (original, entry) in &manifest.entries {
                    let source = mapped(original, &item.source, &held);
                    verify_entry(&source, entry, &cancel)?;
                    if let Some(children) = &entry.children
                        && *children != source_children(&source, &cancel, true)?
                    {
                        return Err(changed(&source).into());
                    }
                    let target = target_path(original)?;
                    let copied = target_manifest
                        .entries
                        .get(&target)
                        .ok_or_else(|| changed(&target))?;
                    let same_children = if target_paths.is_some() {
                        // The compatible copier already verified the exact
                        // output tree and the one-to-one mapping of every row.
                        entry.children.is_some() == copied.children.is_some()
                    } else {
                        entry.children == copied.children
                    };
                    if !same_children {
                        return Err(changed(&target).into());
                    }
                }
                // Reverse path order visits children before their parents.
                // Never remove an unenumerated entry or recursively delete.
                for (index, (original, entry)) in manifest.entries.iter().enumerate().rev() {
                    let source = mapped(original, &item.source, &held);
                    let target = target_path(original)?;
                    let copied = &target_manifest.entries[&target];
                    progress.item(original.path());
                    observe(&source, Phase::BeforeRemove)?;
                    cancel.check()?;
                    if !directory_matches(&staging_meta, &metadata(staging)?) {
                        return Err(changed(staging).into());
                    }
                    verify_ancestors(original, manifest, &item.source, &held, &cancel)?;
                    verify_ancestors(
                        &target,
                        &target_manifest,
                        &item.target,
                        &item.target,
                        &cancel,
                    )?;
                    let finish_attributes =
                        finish_local_attributes || original.remote() || target.remote();
                    let attributes =
                        finish_attributes.then(|| source_plan.attributes(&entry.metadata, index));
                    if entry.children.is_some() {
                        if !directory_matches(&entry.metadata, &metadata(&source)?) {
                            return Err(changed(&source).into());
                        }
                        copied.verify(&target, &cancel, true)?;
                        if let Some(wanted) = attributes {
                            let warning =
                                copy_metadata::finish_directory(&target, &wanted, copied)?;
                            progress.results.note_metadata(original, warning.as_deref());
                            cancel.check()?;
                            if !directory_matches(&entry.metadata, &metadata(&source)?) {
                                return Err(changed(&source).into());
                            }
                        }
                        if let Some(local) = source.local() {
                            fs::remove_dir(local).map_err(err)?;
                        } else {
                            remote(&source)?.rmdir(source.path()).map_err(err)?;
                        }
                    } else {
                        if let Some(link) = links.get(original) {
                            verify_entry(&source, entry, &cancel)?;
                            verify_entry(&target, copied, &cancel)?;
                            if fs::read_link(source.path()).map_err(err)? != *link
                                || fs::read_link(target.path()).map_err(err)? != *link
                            {
                                return Err(changed(&source).into());
                            }
                        } else {
                            verify_file_pair(
                                &source, &target, original, entry, copied, attributes, progress,
                            )?;
                        }
                        cancel.check()?;
                        if let Some(local) = source.local() {
                            fs::remove_file(local).map_err(err)?;
                        } else {
                            remote(&source)?.unlink(source.path()).map_err(err)?;
                        }
                    }
                    progress.results.moved_entry(original, true, None);
                    if original == &item.source {
                        fully_removed = true;
                    }
                    observe(&source, Phase::Removed)?;
                }
                Ok(())
            })();
            if fully_removed {
                *preserve = false;
                return deletion;
            }
            // Restore the untouched or partially removed source tree without
            // replacing a new object that appeared at its original name.
            match rename_for_publication(&held, &item.source) {
                Ok(()) => {
                    *preserve = false;
                    deletion
                }
                Err(restore) => {
                    let reason = deletion
                        .err()
                        .map_or_else(|| changed(&held), |e| e.to_string());
                    let source = &item.source;
                    Err(crate::tr_format!(
                        "{reason}. Remaining source retained at {held}; restoration to {source} failed: {restore}", held = held, reason = reason, restore = restore, source = source
                    ).into())
                }
            }
        })
    })();
    if fully_removed {
        removed();
        return result;
    }
    Err(incomplete(
        &item.target,
        result.err().unwrap_or_else(|| changed(&item.source).into()),
        progress,
    ))
}

pub(super) fn incomplete(
    target: &Location,
    error: TransferError,
    progress: &mut Progress,
) -> TransferError {
    if progress.cancellation().is_requested() {
        progress.results.cancelled = true;
    }
    crate::tr_format!(
        "A copy was created at {target}. The move is incomplete. Check the source and result history. {error}", error = error, target = target
    ).into()
}
