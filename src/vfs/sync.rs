//! Folder synchronization from reviewed, revalidated snapshots.
mod apply;
mod exclusions;
mod two_way;

pub(crate) use exclusions::Exclusions;
#[cfg(test)]
pub(super) use two_way::two_way_round_trip;
pub(crate) use two_way::{Reconcile, ReconcileConflict, TwoWayPlan};

use super::{
    compare::{scan, MAX_ENTRIES, MAX_PATH_BYTES},
    guarded_directory::stale,
    read_scope::{changed_during_read, ReadScope},
    transfer::{
        entry_metadata, validate_source_child_name, EntryMetadata, TransferSourceEntry,
        SYNC_STAGING_PREFIX,
    },
    Location, MAX_TREE_DEPTH,
};
use crate::{
    i18n::text,
    operations::{Cancellation, Progress, TransferError},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(super) use tests::sync_round_trip;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SyncChange {
    Copy,
    Replace,
    Delete,
}

pub(crate) struct SyncRow {
    pub(crate) path: PathBuf,
    pub(crate) change: SyncChange,
    destination: usize,
}

pub(crate) struct SyncPlan {
    folders: [Location; 2],
    observed: [Snapshot; 2],
    exclusions: Exclusions,
    pub(crate) rows: Vec<SyncRow>,
    pub(crate) copies: usize,
    pub(crate) replacements: usize,
    pub(crate) deletions: usize,
}

impl SyncPlan {
    /// The folder order is source followed by destination.
    pub(crate) fn build(
        folders: [Location; 2],
        delete_extra: bool,
        exclusions: Exclusions,
        progress: &mut Progress,
    ) -> Result<Self, TransferError> {
        let (_, observed) = observe_folders(&folders, &exclusions, progress)?;
        let mut plan = Self {
            folders,
            observed,
            exclusions,
            rows: Vec::new(),
            copies: 0,
            replacements: 0,
            deletions: 0,
        };
        let [source, destination] = &plan.observed;
        let paths: BTreeSet<_> = source
            .entries
            .keys()
            .chain(destination.entries.keys())
            .collect();
        let mut new_names = BTreeSet::new();
        let cancel = progress.cancellation();
        for path in paths
            .into_iter()
            .filter(|path| !path.as_os_str().is_empty())
        {
            cancel.check()?;
            let previous = destination.entries.get(path);
            let change = match (source.entries.get(path), previous) {
                (Some(left), Some(right)) if left.kind != right.kind => {
                    plan.replacements += 1;
                    SyncChange::Replace
                }
                (Some(_), None) => {
                    check_new_name(destination, path, &mut new_names)?;
                    plan.copies += 1;
                    SyncChange::Copy
                }
                (None, Some(_)) if delete_extra || above_is_replaced(source, path) => {
                    plan.deletions += 1;
                    SyncChange::Delete
                }
                _ => continue,
            };
            if change != SyncChange::Copy {
                destination.protect_excluded(path)?;
            }
            if source.root.remote() || destination.root.remote() {
                for name in path.iter() {
                    validate_source_child_name(&source.root, name)?;
                }
            }
            plan.rows.push(SyncRow {
                path: path.clone(),
                change,
                destination: 1,
            });
        }
        Ok(plan)
    }

    pub(crate) fn verify(&self, progress: &mut Progress) -> Result<(), TransferError> {
        self.revalidate(progress).map(drop)
    }

    fn revalidate(&self, progress: &mut Progress) -> Result<[ReadScope; 2], TransferError> {
        revalidate_folders(&self.folders, &self.observed, &self.exclusions, progress)
    }

    pub(crate) fn excluded_count(&self) -> usize {
        self.observed
            .iter()
            .map(|snapshot| snapshot.excluded.len())
            .sum()
    }
}

fn above_is_replaced(source: &Snapshot, path: &Path) -> bool {
    // A file or link replacing a destination directory also removes its
    // descendants, even when unrelated destination-only paths are preserved.
    path.ancestors().skip(1).any(|parent| {
        source
            .entries
            .get(parent)
            .is_some_and(|entry| entry.kind != Kind::Directory)
    })
}

fn check_new_name(
    destination: &Snapshot,
    path: &Path,
    names: &mut BTreeSet<(PathBuf, String)>,
) -> Result<(), TransferError> {
    let name = path.file_name().ok_or_else(changed_during_read)?;
    let parent = path.parent().ok_or_else(changed_during_read)?;
    let location = destination.root.join(path);
    if let Some(name) = name.to_str()
        && !names.insert((
            parent.to_owned(),
            crate::name_match::normalize(name).into_owned(),
        ))
    {
        return Err(name_collision(&location));
    }
    if destination
        .entries
        .get(parent)
        .is_some_and(|entry| entry.kind == Kind::Directory)
        && entry_metadata(&location)?.is_some()
    {
        // A differently spelled name can occupy the slot on this filesystem
        // even though the exact name was absent from its directory listing.
        return Err(name_collision(&location));
    }
    Ok(())
}

fn name_collision(location: &Location) -> TransferError {
    crate::tr_format!(
        "Names collide at the destination: {location}",
        location = location
    )
    .into()
}

fn is_sync_staging(name: &OsStr) -> bool {
    let Some(suffix) = name
        .to_str()
        .and_then(|name| name.strip_prefix(SYNC_STAGING_PREFIX))
    else {
        return false;
    };
    let mut parts = suffix.split('-');
    (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    }) && parts.next().is_none()
}

fn retained_stage(location: &Location) -> TransferError {
    crate::tr_format!(
        "Found a sync staging folder: {location}. Wait for any running synchronization to finish. If it was interrupted, inspect the retained versions before building a new plan.", location = location
    ).into()
}

#[derive(Clone, Eq, PartialEq)]
enum Kind {
    Directory,
    File([u8; 32]),
    Link(OsString),
}

struct Observed {
    kind: Kind,
    version: TransferSourceEntry,
}

struct Snapshot {
    root: Location,
    entries: BTreeMap<PathBuf, Observed>,
    excluded: BTreeSet<PathBuf>,
}

fn revalidate_folders(
    folders: &[Location; 2],
    observed: &[Snapshot; 2],
    exclusions: &Exclusions,
    progress: &mut Progress,
) -> Result<[ReadScope; 2], TransferError> {
    let (scopes, current) = observe_folders(folders, exclusions, progress)?;
    let cancel = progress.cancellation();
    for (before, after) in observed.iter().zip(current) {
        if before.root != after.root
            || before.entries.len() != after.entries.len()
            || before.excluded != after.excluded
        {
            return Err(stale(&before.root));
        }
        for (path, entry) in &before.entries {
            cancel.check()?;
            let location = before.root.join(path);
            let actual = after.entries.get(path).ok_or_else(|| stale(&location))?;
            if entry.kind != actual.kind
                || entry
                    .version
                    .verify_metadata(&location, &actual.version.metadata)
                    .is_err()
            {
                return Err(stale(&location));
            }
        }
    }
    Ok(scopes)
}

fn observe_folders(
    folders: &[Location; 2],
    exclusions: &Exclusions,
    progress: &mut Progress,
) -> Result<([ReadScope; 2], [Snapshot; 2]), TransferError> {
    let cancel = progress.cancellation();
    cancel.check()?;
    let source = ReadScope::open(&folders[0])?;
    cancel.check()?;
    let destination = ReadScope::open(&folders[1])?;
    check_roots(
        &source.parent.location,
        &destination.parent.location,
        &cancel,
    )?;
    let mut budget = Budget::default();
    let observed = [
        Snapshot::read(&source, exclusions, &mut budget, progress)?,
        Snapshot::read(&destination, exclusions, &mut budget, progress)?,
    ];
    Ok(([source, destination], observed))
}

fn check_roots(
    source: &Location,
    destination: &Location,
    cancel: &Cancellation,
) -> Result<(), TransferError> {
    let overlap = || text!("Источник и назначение синхронизации пересекаются").into();
    if (source.might_share_filesystem(destination)
        || source
            .sftp_endpoint()
            .zip(destination.sftp_endpoint())
            .is_some_and(|(a, b)| a.same_authority(b)))
        && (source.path().starts_with(destination.path())
            || destination.path().starts_with(source.path()))
    {
        return Err(overlap());
    }
    if source.local().is_some() && destination.local().is_some() {
        // Check directory identity too. A local alias or volume spelling
        // mismatch can hide an overlap from a lexical path comparison.
        for (parent, child) in [(source, destination), (destination, source)] {
            cancel.check()?;
            let expected = entry_metadata(parent)?.ok_or_else(changed_during_read)?;
            for ancestor in child.ancestors() {
                cancel.check()?;
                if entry_metadata(&ancestor)?.is_some_and(|actual| expected.same_object(&actual)) {
                    return Err(overlap());
                }
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct Budget {
    entries: usize,
    bytes: usize,
}

impl Budget {
    fn visit(&mut self, bytes: usize) -> Result<(), TransferError> {
        self.entries += 1;
        self.bytes += bytes;
        if self.entries > MAX_ENTRIES || self.bytes > MAX_PATH_BYTES {
            return Err(text!(
                "Достигнут предел плана: 100 000 объектов или 32 МиБ данных о путях"
            )
            .into());
        }
        Ok(())
    }
}

impl Snapshot {
    fn empty(scope: &ReadScope) -> Self {
        Self {
            root: scope.parent.location.clone(),
            entries: BTreeMap::new(),
            excluded: BTreeSet::new(),
        }
    }

    fn protect_excluded(&self, path: &Path) -> Result<(), TransferError> {
        if self.excluded.is_empty() {
            return Ok(());
        }
        if let Some(excluded) = self.excluded.range(path.to_owned()..).next()
            && excluded.starts_with(path)
        {
            let location = self.root.join(path);
            let excluded = excluded.display();
            return Err(crate::tr_format!(
                "Replacing or deleting {location} would affect the excluded path {excluded}.",
                excluded = excluded,
                location = location
            )
            .into());
        }
        Ok(())
    }

    fn subtree<'a>(&'a self, path: &'a Path) -> impl Iterator<Item = (&'a PathBuf, &'a Observed)> {
        self.entries
            .range(path.to_owned()..)
            .take_while(move |(candidate, _)| candidate.starts_with(path))
    }

    fn read(
        scope: &ReadScope,
        exclusions: &Exclusions,
        budget: &mut Budget,
        progress: &mut Progress,
    ) -> Result<Self, TransferError> {
        let mut snapshot = Self::empty(scope);
        snapshot.observe(
            scope,
            PathBuf::new(),
            scan::Entry::Directory,
            exclusions,
            budget,
            progress,
        )?;
        scope.parent.verify()?;
        Ok(snapshot)
    }

    fn observe(
        &mut self,
        scope: &ReadScope,
        relative: PathBuf,
        entry: scan::Entry,
        exclusions: &Exclusions,
        budget: &mut Budget,
        progress: &mut Progress,
    ) -> Result<(), TransferError> {
        let cancel = progress.cancellation();
        cancel.check()?;
        if let scan::Entry::Unreadable(error) = entry {
            return Err(error);
        }
        let location = self.root.join(&relative);
        let metadata = entry_metadata(&location)?.ok_or_else(changed_during_read)?;
        let kind = match entry {
            scan::Entry::Directory if metadata.is_directory() => {
                if relative.components().count() > MAX_TREE_DEPTH {
                    return Err(text!("Достигнут предел вложенности папок").into());
                }
                progress.item(location.path());
                let mut children = BTreeMap::new();
                scan::visit(scope, &relative, &cancel, &mut |name, child| {
                    let path = relative.join(&name);
                    if is_sync_staging(&name) {
                        return Err(retained_stage(&self.root.join(&path)));
                    }
                    if exclusions.skips(&path) {
                        budget.visit(path.as_os_str().len())?;
                        self.excluded.insert(path);
                        return Ok(());
                    }
                    budget.visit(path.as_os_str().len() + child.extra_path_bytes())?;
                    if children.insert(path, child).is_some() {
                        return Err(text!("Папка содержит повторяющиеся имена").into());
                    }
                    Ok(())
                })?;
                for (path, child) in children {
                    self.observe(scope, path, child, exclusions, budget, progress)?;
                }
                Kind::Directory
            }
            scan::Entry::File(_) if metadata.is_file() => Kind::File(
                scope
                    .digest(&relative, progress, &cancel)?
                    .ok_or_else(changed_during_read)?,
            ),
            scan::Entry::Link(target) if is_link(&metadata) => Kind::Link(target.into_os_string()),
            _ => return Err(changed_during_read()),
        };
        let version = TransferSourceEntry {
            metadata,
            children: None,
        };
        version.verify(&location, &cancel, false)?;
        self.entries.insert(relative, Observed { kind, version });
        Ok(())
    }
}

fn is_link(metadata: &EntryMetadata) -> bool {
    match metadata {
        EntryMetadata::Local(meta) => meta.is_symlink(),
        EntryMetadata::Remote(stat) => stat.file_type().is_symlink(),
    }
}
