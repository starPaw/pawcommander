//! Verified copy followed by guarded, nonrecursive source removal.
//! A published copy and a completed move are separate facts. Partial moves
//! are never automatic retry or inverse-rename candidates.
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
    Quarantined,
    BeforeRemove,
    Removed,
}

fn changed(path: &Location) -> String {
    crate::tr_format!(
        "Перенос остановлен: объект {path} изменился или недоступен",
        "Move stopped: item {path} changed or is inaccessible"
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
    let cancel = progress.cancellation();
    let scan = cancel.clone().pausable_scan();
    let manifest =
        TransferSourceManifest::capture_move_observed(&item.source, &scan, &mut |_, _| {})?;
    let mut links = BTreeMap::<Location, PathBuf>::new();
    for (path, entry) in &manifest.entries {
        if entry.children.is_none() && !entry.metadata.is_file() {
            // Local symbolic links can be compared without following them.
            // Remote links/special objects retain the existing copy restriction.
            if let (Some(local), Some(_), EntryMetadata::Local(meta)) =
                (path.local(), item.target.local(), &entry.metadata)
                && meta.is_symlink()
                && entry.metadata.same_object(&entry.metadata)
            {
                links.insert(path.clone(), fs::read_link(local).map_err(err)?);
            } else {
                return Err(crate::tr_format!(
                    "Безопасный перенос этого типа объекта между дисками недоступен: {path}",
                    "Safe cross-volume move is unavailable for this object type: {path}"
                )
                .into());
            }
        }
    }
    let configured = progress.copy_verification();
    progress.set_copy_verification(true);
    let mut copied = false;
    let result = copy_item_cancellable(item, expected, progress, || {
        copied = true;
        published();
    });
    progress.set_copy_verification(configured);
    if !copied {
        return result;
    }
    progress.results.copied_source_remaining();
    let mut fully_removed = false;
    let result = result.and_then(|()| {
        observe(&item.source, Phase::Copied)?;
        progress.checkpoint()?;
        manifest.verify(&cancel, |_, _| {})?;
        let target_manifest =
            TransferSourceManifest::capture_move_observed(&item.target, &cancel, &mut |_, _| {})?;
        with_preserved_staging(&item.source, &cancel, |staging, preserve| {
            let staging_meta = metadata(staging)?;
            let held = staging.join("source");
            if let Some(retry) = &mut progress.retry {
                retry.note_held_source(&item.original, &held);
            }
            // This directory is registered for crash recovery and retained
            // even on panic or an unacknowledged remote rename.
            if let Err(error) = rename_for_publication(&item.source, &held) {
                return Err(crate::tr_format!(
                    "{error}. Проверьте источник {} и сохранённую папку {held}",
                    "{error}. Inspect source {} and retained folder {held}",
                    item.source
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
                    let target = mapped(original, &item.source, &item.target);
                    let copied = target_manifest
                        .entries
                        .get(&target)
                        .ok_or_else(|| changed(&target))?;
                    if entry.children != copied.children {
                        return Err(changed(&target).into());
                    }
                }
                // Reverse path order visits children before their parents.
                // Never remove an unenumerated entry or recursively delete.
                for (original, entry) in manifest.entries.iter().rev() {
                    let source = mapped(original, &item.source, &held);
                    let target = mapped(original, &item.source, &item.target);
                    let copied = &target_manifest.entries[&target];
                    progress.item(original.path());
                    observe(&source, Phase::BeforeRemove)?;
                    cancel.check()?;
                    if !directory_matches(&staging_meta, &metadata(staging)?) {
                        return Err(changed(staging).into());
                    }
                    verify_ancestors(original, &manifest, &item.source, &held, &cancel)?;
                    verify_ancestors(
                        &target,
                        &target_manifest,
                        &item.target,
                        &item.target,
                        &cancel,
                    )?;
                    if entry.children.is_some() {
                        if !directory_matches(&entry.metadata, &metadata(&source)?) {
                            return Err(changed(&source).into());
                        }
                        copied.verify(&target, &cancel, true)?;
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
                            verify_file_pair(&source, &target, original, entry, copied, progress)?;
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
                    Err(crate::tr_format!(
                    "{}. Остаток источника сохранён в {held}; возврат в {} не выполнен: {restore}",
                    "{}. Remaining source retained at {held}; restoration to {} failed: {restore}",
                    deletion.err().map_or_else(|| changed(&held), |e| e.to_string()), item.source
                )
                    .into())
                }
            }
        })
    });
    if fully_removed {
        removed();
        return result;
    }
    if cancel.is_requested() {
        progress.results.cancelled = true;
    }
    Err(crate::tr_format!(
        "Копия создана в {}, но перенос не завершён. Остаток источника сохранён; автоматический повтор недоступен. {}",
        "Copy created at {}, but the move is incomplete. Remaining source was preserved; automatic retry is unavailable. {}",
        item.target, result.err().map_or_else(|| changed(&item.source), |e| e.to_string())
    ).into())
}
