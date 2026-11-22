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
    guarded_directory::{stale, Directory},
    read_scope::ReadScope,
    sftp::remote,
    transfer::{
        entry_metadata, read_entry_metadata, validate_sftp_source_path, EntryMetadata,
        TransferSourceEntry, SYNC_STAGING_PREFIX,
    },
    Location, MAX_TREE_DEPTH,
};
use crate::{
    file_error::{AccessAction, AccessError, ScanError, SyncError, SyncFolderRole},
    operations::{Cancellation, Progress, TransferError},
};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
};

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(super) use tests::sync_round_trip;

const ONE_WAY_ROLES: [SyncFolderRole; 2] = [SyncFolderRole::Source, SyncFolderRole::Destination];
const TWO_WAY_ROLES: [SyncFolderRole; 2] = [SyncFolderRole::Left, SyncFolderRole::Right];

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

pub(crate) struct SyncReport {
    pub(crate) completed: usize,
    pub(crate) result: Result<(), TransferError>,
}

impl SyncPlan {
    /// The folder order is source followed by destination.
    pub(crate) fn build(
        folders: [Location; 2],
        delete_extra: bool,
        exclusions: Exclusions,
        progress: &mut Progress,
    ) -> Result<Self, TransferError> {
        let (_, observed) = observe_folders(&folders, ONE_WAY_ROLES, &exclusions, progress)?;
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
                validate_sftp_source_path(&source.root, path)?;
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
        revalidate_folders(
            &self.folders,
            ONE_WAY_ROLES,
            &self.observed,
            &self.exclusions,
            progress,
        )
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
    let location = destination.root.join(path);
    let name = path.file_name().ok_or_else(|| stale(&location))?;
    let parent = path.parent().ok_or_else(|| stale(&location))?;
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
    SyncError::NameCollision(location.clone()).into()
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
    SyncError::RetainedStage(location.clone()).into()
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
    roles: [SyncFolderRole; 2],
    observed: &[Snapshot; 2],
    exclusions: &Exclusions,
    progress: &mut Progress,
) -> Result<[ReadScope; 2], TransferError> {
    let (scopes, current) = observe_folders(folders, roles, exclusions, progress)?;
    let cancel = progress.cancellation();
    for (side, (before, after)) in observed.iter().zip(current).enumerate() {
        before
            .verify_unchanged(&after, &cancel)
            .map_err(|error| folder_error(roles[side], &folders[side], error))?;
    }
    Ok(scopes)
}

fn observe_folders(
    folders: &[Location; 2],
    roles: [SyncFolderRole; 2],
    exclusions: &Exclusions,
    progress: &mut Progress,
) -> Result<([ReadScope; 2], [Snapshot; 2]), TransferError> {
    let cancel = progress.cancellation();
    let context = |side: usize, error| folder_error(roles[side], &folders[side], error);
    let open = |side: usize| -> Result<_, TransferError> {
        cancel.check()?;
        let scope = ReadScope::open(&folders[side]).map_err(|error| context(side, error))?;
        if scope.descriptor.is_some() {
            Directory::from_scope(&scope).map_err(|error| context(side, error))?;
        }
        Ok(scope)
    };
    let scopes = [open(0)?, open(1)?];
    check_roots(
        scopes.each_ref().map(|scope| &scope.parent.location),
        roles,
        &cancel,
    )?;
    let mut budget = Budget::default();
    let observed = [
        Snapshot::read(&scopes[0], exclusions, &mut budget, progress)
            .map_err(|error| context(0, error))?,
        Snapshot::read(&scopes[1], exclusions, &mut budget, progress)
            .map_err(|error| context(1, error))?,
    ];
    // The first folder may have moved while the second was being read.
    for (side, scope) in scopes.iter().enumerate() {
        cancel.check()?;
        scope
            .verify_selected_root(&folders[side])
            .map_err(|error| context(side, error))?;
    }
    Ok((scopes, observed))
}

fn folder_error(role: SyncFolderRole, folder: &Location, cause: TransferError) -> TransferError {
    match cause {
        cause @ TransferError::Failed(_) => SyncError::Folder {
            role,
            folder: folder.clone(),
            cause: Box::new(cause),
        }
        .into(),
        cause => cause,
    }
}

fn read_metadata(location: &Location) -> Result<EntryMetadata, TransferError> {
    read_entry_metadata(location)
        .map_err(|cause| AccessError::new(location, AccessAction::ReadMetadata, cause).into())
}

fn check_roots(
    folders: [&Location; 2],
    roles: [SyncFolderRole; 2],
    cancel: &Cancellation,
) -> Result<(), TransferError> {
    let [source, destination] = folders;
    let overlap = || SyncError::Overlap {
        source: source.clone(),
        destination: destination.clone(),
    };
    if (source.might_share_filesystem(destination)
        || source
            .sftp_endpoint()
            .zip(destination.sftp_endpoint())
            .is_some_and(|(a, b)| a.same_authority(b)))
        && (source.path().starts_with(destination.path())
            || destination.path().starts_with(source.path()))
    {
        return Err(overlap().into());
    }
    if source.local().is_some() && destination.local().is_some() {
        // Check directory identity too. A local alias or volume spelling
        // mismatch can hide an overlap from a lexical path comparison.
        for side in 0..2 {
            cancel.check()?;
            let folder = folders[side];
            let other = folders[1 - side];
            let expected =
                read_metadata(folder).map_err(|error| folder_error(roles[side], folder, error))?;
            for ancestor in other.ancestors() {
                cancel.check()?;
                let actual = entry_metadata(&ancestor)
                    .map_err(|error| folder_error(roles[1 - side], other, error))?;
                if actual.is_some_and(|actual| expected.same_object(&actual)) {
                    return Err(overlap().into());
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
            return Err(SyncError::PlanLimit.into());
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
            return Err(SyncError::ExcludedPath {
                target: self.root.join(path),
                excluded: excluded.clone(),
            }
            .into());
        }
        Ok(())
    }

    fn subtree<'a>(&'a self, path: &'a Path) -> impl Iterator<Item = (&'a PathBuf, &'a Observed)> {
        self.entries
            .range(path.to_owned()..)
            .take_while(move |(candidate, _)| candidate.starts_with(path))
    }

    fn verify_unchanged(&self, after: &Self, cancel: &Cancellation) -> Result<(), TransferError> {
        cancel.check()?;
        let root = Path::new("");
        if self.root != after.root
            || (self.root.local().is_some()
                && !self.entries[root]
                    .version
                    .metadata
                    .same_object(&after.entries[root].version.metadata))
        {
            return Err(stale(&self.root));
        }
        let mut current = after.entries.iter();
        let mut changed_folder = None;
        for (path, before) in &self.entries {
            cancel.check()?;
            let location = self.root.join(path);
            let Some((actual_path, actual)) = current.next() else {
                return Err(SyncError::PlanEntryMissing(location).into());
            };
            match path.cmp(actual_path) {
                Ordering::Less => return Err(SyncError::PlanEntryMissing(location).into()),
                Ordering::Greater => {
                    return Err(SyncError::PlanEntryAdded(self.root.join(actual_path)).into())
                }
                Ordering::Equal => {}
            }
            if before.kind != actual.kind {
                return Err(stale(&location));
            }
            if !before
                .version
                .matches_metadata(&actual.version.metadata)
                .map_err(|cause| AccessError::new(&location, AccessAction::ReadMetadata, cause))?
            {
                if before.kind != Kind::Directory {
                    return Err(stale(&location));
                }
                // A changed child also updates its parent. Prefer the exact child path.
                changed_folder.get_or_insert(location);
            }
        }
        if let Some((path, _)) = current.next() {
            return Err(SyncError::PlanEntryAdded(self.root.join(path)).into());
        }
        if let Some(path) = self.excluded.symmetric_difference(&after.excluded).next() {
            let location = self.root.join(path);
            let error = if self.excluded.contains(path) {
                SyncError::PlanEntryMissing(location)
            } else {
                SyncError::PlanEntryAdded(location)
            };
            return Err(error.into());
        }
        if let Some(folder) = changed_folder {
            return Err(stale(&folder));
        }
        Ok(())
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
        let metadata = read_metadata(&location)?;
        let kind = match entry {
            scan::Entry::Directory if metadata.is_directory() => {
                if relative.components().count() > MAX_TREE_DEPTH {
                    return Err(ScanError::DepthLimit(location).into());
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
                        return Err(ScanError::DuplicateEntry {
                            directory: location.clone(),
                            name: name.to_os_string(),
                        }
                        .into());
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
                    .require_file(&relative, &cancel)?
                    .digest(progress, &cancel)?,
            ),
            scan::Entry::Link(target) if is_link(&metadata) => {
                if read_link(&location)? != target {
                    return Err(SyncError::LinkTargetChanged(location).into());
                }
                Kind::Link(target.into_os_string())
            }
            _ => return Err(SyncError::EntryTypeChanged(location).into()),
        };
        let version = TransferSourceEntry {
            metadata,
            children: None,
        };
        cancel.check()?;
        version.verify_metadata(&location, &read_metadata(&location)?)?;
        self.entries.insert(relative, Observed { kind, version });
        Ok(())
    }
}

fn read_link(location: &Location) -> Result<PathBuf, TransferError> {
    if let Some(path) = location.local() {
        fs::read_link(path)
            .map_err(|cause| AccessError::new(location, AccessAction::ReadLink, cause).into())
    } else {
        remote(location)?
            .readlink(location.path())
            .map_err(|cause| AccessError::new(location, AccessAction::ReadLink, cause).into())
    }
}

fn is_link(metadata: &EntryMetadata) -> bool {
    match metadata {
        EntryMetadata::Local(meta) => meta.is_symlink(),
        EntryMetadata::Remote(stat) => stat.file_type().is_symlink(),
    }
}
