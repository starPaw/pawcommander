use super::{
    folder_error, is_link, read_link, read_metadata, stale, Budget, Exclusions, Kind, Observed,
    Snapshot, SyncChange, SyncPlan, SyncReport, SyncRow, ONE_WAY_ROLES,
};
use crate::{
    file_error::{
        AccessAction, AccessError, CopyVerificationError, LocalPathAction, MetadataWarning,
        MutationError, PublicationError, StagingCleanupError, SyncDifference, SyncError,
        SyncFolderRole, SyncVerification,
    },
    operations::{AccessStream, Cancellation, Progress, TransferError},
    result_history::{Outcome, Results},
    vfs::{
        compare::scan,
        copy_metadata,
        guarded_directory::{
            open_bound_staging, open_staging, retained, uncertain_directory, Directory,
        },
        read_scope::ReadScope,
        sftp::remote,
        staged_file::StagedFile,
        transfer::{
            canonical, with_open_sync_staging, with_preserved_staging, with_tracked_staging,
            EntryMetadata, SourceReader, StagingNotice, TransferSourceEntry,
        },
        Location,
    },
};
use std::{cmp::Ordering, ffi::OsStr, fs, io::Read, os::unix::fs::DirBuilderExt, path::Path};

struct GroupWarnings {
    metadata: Option<MetadataWarning>,
    cleanup: Option<TransferError>,
}

impl SyncPlan {
    pub(crate) fn folders(&self) -> &[Location; 2] {
        &self.folders
    }

    pub(crate) fn apply(&self, progress: &mut Progress) -> SyncReport {
        let execution = Execution {
            folders: &self.folders,
            roles: ONE_WAY_ROLES,
            observed: &self.observed,
            rows: &self.rows,
            exclusions: &self.exclusions,
        };
        execution.plan_history(progress);
        let mut completed = 0;
        let result = self
            .revalidate(progress)
            .and_then(|scopes| execution.apply_groups(&scopes, progress, &mut completed, None));
        finish(result, completed, progress)
    }
}

pub(super) fn finish(
    result: Result<(), TransferError>,
    completed: usize,
    progress: &mut Progress,
) -> SyncReport {
    if matches!(result, Err(TransferError::Cancelled)) {
        progress.results.cancelled = true;
    }
    SyncReport { completed, result }
}

pub(super) struct Execution<'a> {
    pub(super) folders: &'a [Location; 2],
    pub(super) roles: [SyncFolderRole; 2],
    pub(super) observed: &'a [Snapshot; 2],
    pub(super) rows: &'a [SyncRow],
    pub(super) exclusions: &'a Exclusions,
}

impl Execution<'_> {
    pub(super) fn plan_history(&self, progress: &mut Progress) {
        progress
            .results
            .plan(self.rows.iter().map(|row| self.history_row(row)));
    }

    fn history_row(&self, row: &SyncRow) -> (Location, Option<String>) {
        let destination = self.observed[row.destination].root.join(&row.path);
        if row.change == SyncChange::Delete {
            (destination, None)
        } else {
            (
                self.observed[1 - row.destination].root.join(&row.path),
                Some(destination.display()),
            )
        }
    }

    fn verify_smb_scopes(&self, scopes: &[ReadScope; 2]) -> Result<(), TransferError> {
        for (side, scope) in scopes.iter().enumerate() {
            if scope.owned_smb_mount().is_some() {
                let folder = &self.folders[side];
                scope
                    .verify_selected_root(folder)
                    .map_err(|cause| folder_error(self.roles[side], folder, cause))?;
            }
        }
        Ok(())
    }

    pub(super) fn apply_groups(
        &self,
        scopes: &[ReadScope; 2],
        progress: &mut Progress,
        completed: &mut usize,
        mut staging: Option<&mut StagingNotice<'_>>,
    ) -> Result<(), TransferError> {
        let cancel = progress.cancellation();
        let mut groups = groups(self.rows);
        groups.sort_by_key(|group| group[0].change == SyncChange::Delete);
        for group in groups {
            cancel.check()?;
            let row = &group[0];
            let (key, _) = self.history_row(row);
            progress.results.begin(&key);
            let result = (|| {
                for (side, (input, scope)) in self.folders.iter().zip(scopes).enumerate() {
                    cancel.check()?;
                    scope
                        .verify_selected_root(input)
                        .map_err(|cause| folder_error(self.roles[side], input, cause))?;
                }
                let source = 1 - row.destination;
                let destination = row.destination;
                progress.set_copy_endpoints(
                    &self.observed[source].root,
                    &self.observed[destination].root,
                );
                self.apply_group(row, scopes, progress, staging.as_deref_mut())
            })();
            match result {
                Ok(warnings) => {
                    self.record_completed_group(group, warnings, &mut progress.results);
                    *completed += group.len();
                }
                Err(error) => {
                    let outcome = match &error {
                        TransferError::Cancelled => Outcome::Cancelled,
                        TransferError::Failed(_) => Outcome::Failed,
                        TransferError::PublicationUnconfirmed(_) => Outcome::PublicationUnconfirmed,
                    };
                    progress
                        .results
                        .record_with_error(&key, outcome, Some(&error));
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn record_completed_group(
        &self,
        group: &[SyncRow],
        mut warnings: GroupWarnings,
        results: &mut Results,
    ) {
        for row in group {
            let (key, target) = self.history_row(row);
            results.record_sync_completion(
                &key,
                target,
                warnings.metadata.take(),
                warnings.cleanup.take(),
            );
        }
    }

    fn apply_group(
        &self,
        row: &SyncRow,
        scopes: &[ReadScope; 2],
        progress: &mut Progress,
        notice: Option<&mut StagingNotice<'_>>,
    ) -> Result<GroupWarnings, TransferError> {
        let source_scope = &scopes[1 - row.destination];
        let destination_scope = &scopes[row.destination];
        let destination_mount = destination_scope.owned_smb_mount();
        let source = &self.observed[1 - row.destination];
        let destination = &self.observed[row.destination];
        let relative = row.path.as_path();
        let target = destination.root.join(relative);
        let parent_path = relative.parent().ok_or_else(|| stale(&target))?;
        let name = relative.file_name().ok_or_else(|| stale(&target))?;
        let cancel = progress.cancellation();
        let target_parent = destination.root.join(parent_path);
        let parent_entry = destination
            .entries
            .get(parent_path)
            .ok_or_else(|| stale(&target_parent))?;
        let parent = Directory::at(
            destination_scope,
            parent_path,
            &parent_entry.version.metadata,
            &cancel,
        )
        .map_err(|cause| folder_error(self.roles[row.destination], &target_parent, cause))?;
        let deletion_source = if row.change == SyncChange::Delete {
            let source_parent = source.root.join(parent_path);
            let entry = source
                .entries
                .get(parent_path)
                .ok_or_else(|| stale(&source_parent))?;
            Some(
                Directory::at(source_scope, parent_path, &entry.version.metadata, &cancel)
                    .map_err(|cause| {
                        folder_error(self.roles[1 - row.destination], &source_parent, cause)
                    })?,
            )
        } else {
            None
        };
        let mut committed = false;
        let mut metadata_warning = None;
        // Only a source subtree is filtered. A temporary copy and every
        // subtree being replaced or removed must match in full.
        let unfiltered = Exclusions::default();
        let mut work = |staging: &Location, opened: Option<&fs::File>, preserve: &mut bool| {
            *preserve = false;
            parent.verify()?;
            let stage_scope = if let Some(mount) = destination_mount {
                let handle = opened.ok_or_else(|| stale(staging))?;
                open_bound_staging(staging, mount, handle)
                    .map_err(|cause| uncertain_directory(staging, cause))?
            } else {
                open_staging(staging)?
            };
            let stage = Directory::from_scope(&stage_scope)?;
            let incoming = Path::new("incoming");
            if row.change != SyncChange::Delete {
                stage_source(source, relative, source_scope, &stage_scope, progress)?;
                let staged = required_tree(&stage_scope, incoming, &unfiltered, progress)?;
                check_tree(
                    source,
                    relative,
                    &staged,
                    incoming,
                    Check::Contents,
                    &cancel,
                )?;
                let current = required_tree(source_scope, relative, self.exclusions, progress)?;
                check_tree(
                    source,
                    relative,
                    &current,
                    relative,
                    Check::Original,
                    &cancel,
                )?;
                metadata_warning = preserve_attributes(
                    source,
                    relative,
                    &staged,
                    incoming,
                    &stage_scope,
                    progress,
                )?;
            }

            if row.change == SyncChange::Copy {
                self.verify_smb_scopes(scopes)?;
                parent.verify()?;
                stage.verify()?;
                cancel.check()?;
                match stage.rename(incoming.as_os_str(), &parent, name) {
                    Ok(()) => {}
                    Err(error @ TransferError::PublicationUnconfirmed(_)) => {
                        *preserve = true;
                        return Err(retained(error, staging));
                    }
                    Err(error) => return Err(error),
                }
            } else {
                let current = required_tree(destination_scope, relative, &unfiltered, progress)?;
                check_tree(
                    destination,
                    relative,
                    &current,
                    relative,
                    Check::Original,
                    &cancel,
                )?;
                if let Some(source) = &deletion_source {
                    source.verify_absent(name)?;
                }
                let backup_folder = staging.join("previous");
                let backup_scope = if let Some(mount) = destination_mount {
                    let backup = stage.exclusive_directory(OsStr::new("previous"))?;
                    let handle = backup
                        .descriptor
                        .as_ref()
                        .ok_or_else(|| stale(&backup_folder))?;
                    open_bound_staging(&backup_folder, mount, handle)?
                } else {
                    make_directory(&backup_folder)?;
                    open_staging(&backup_folder)?
                };
                let backup = Directory::from_scope(&backup_scope)?;
                self.verify_smb_scopes(scopes)?;
                parent.verify()?;
                backup.verify()?;
                cancel.check()?;
                match parent.rename(name, &backup, name) {
                    Ok(()) => *preserve = true,
                    Err(error @ TransferError::PublicationUnconfirmed(_)) => {
                        *preserve = true;
                        return Err(retained(error, staging));
                    }
                    Err(error) => return Err(error),
                }

                let publication = (|| {
                    let held =
                        required_tree(&backup_scope, Path::new(name), &unfiltered, progress)?;
                    check_tree(
                        destination,
                        relative,
                        &held,
                        Path::new(name),
                        Check::Held,
                        &cancel,
                    )?;
                    self.verify_smb_scopes(scopes)?;
                    parent.verify()?;
                    backup.verify()?;
                    cancel.check()?;
                    if row.change == SyncChange::Replace {
                        stage.verify()?;
                        stage.rename(incoming.as_os_str(), &parent, name)?;
                    } else if let Some(source) = &deletion_source {
                        source.verify_absent(name)?;
                        parent.verify_absent(name)?;
                        if destination_mount.is_some() {
                            verify_held_entry(destination, relative, &backup, name)?;
                        }
                        if let Some(path) = backup.location.local() {
                            let path = path.join(name);
                            trash::delete(&path)
                                .map_err(|source| MutationError::Trash { path, source })?;
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = publication {
                    if matches!(error, TransferError::PublicationUnconfirmed(_)) {
                        return Err(retained(error, staging));
                    }
                    let restore = backup
                        .verify()
                        .and_then(|()| parent.verify())
                        .and_then(|()| {
                            if destination_mount.is_some() {
                                verify_held_entry(destination, relative, &backup, name)?;
                            }
                            Ok(())
                        })
                        .and_then(|()| backup.rename(name, &parent, name));
                    if let Err(restore) = restore {
                        let failure = PublicationError::Rollback {
                            target: target.clone(),
                            backup: backup.location.join(name),
                            cause: error,
                            restore,
                        };
                        return Err(retained(failure.into(), staging));
                    }
                    *preserve = false;
                    return Err(error);
                }
            }

            committed = true;
            *preserve = false;
            Ok(())
        };
        let result = if let Some(mount) = destination_mount {
            parent.verify()?;
            let handle = parent
                .descriptor
                .as_ref()
                .ok_or_else(|| stale(&target_parent))?;
            with_open_sync_staging(
                &target,
                &cancel,
                handle,
                notice,
                |staging, opened, preserve| {
                    let result = work(staging, Some(opened), preserve);
                    if !*preserve {
                        let cleanup = mount.recheck().and_then(|()| {
                            mount.verify_directory(opened, staging.path(), target.path())
                        });
                        if cleanup.is_err() {
                            *preserve = true;
                            return StagingCleanupError::combine(result, cleanup, staging, &target);
                        }
                    }
                    result
                },
            )
        } else {
            let work = |staging: &Location, preserve: &mut bool| work(staging, None, preserve);
            match notice {
                Some(notice) => with_tracked_staging(&target, &cancel, notice, work),
                None => with_preserved_staging(&target, &cancel, work),
            }
        };
        let cleanup = match result {
            Ok(()) => None,
            Err(error) if committed => Some(error),
            Err(error) => return Err(error),
        };
        Ok(GroupWarnings {
            metadata: metadata_warning,
            cleanup,
        })
    }
}

fn verify_held_entry(
    destination: &Snapshot,
    relative: &Path,
    backup: &Directory,
    name: &OsStr,
) -> Result<(), TransferError> {
    backup.verify()?;
    let location = backup.location.join(name);
    let actual = read_metadata(&location)?;
    if !destination.entries[relative]
        .version
        .matches_moved_target(&actual)
    {
        return Err(stale(&location));
    }
    Ok(())
}

fn groups(mut rows: &[SyncRow]) -> Vec<&[SyncRow]> {
    let mut groups = Vec::new();
    while let Some(first) = rows.first() {
        let end = rows
            .iter()
            .position(|row| !row.path.starts_with(&first.path))
            .unwrap_or(rows.len());
        let (group, remainder) = rows.split_at(end);
        groups.push(group);
        rows = remainder;
    }
    groups
}

fn make_directory(location: &Location) -> Result<(), TransferError> {
    if let Some(path) = location.local() {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|error| MutationError::local(path, LocalPathAction::CreateDirectory, error))?;
    } else {
        remote(location)?
            .mkdir(location.path(), 0o700)
            .map_err(|source| MutationError::CreateSftp {
                destination: location.clone(),
                source,
            })?;
    }
    Ok(())
}

fn stage_source(
    source: &Snapshot,
    relative: &Path,
    scope: &ReadScope,
    stage_scope: &ReadScope,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let cancel = progress.cancellation();
    let payload = stage_scope.parent.location.join("incoming");
    let bound = stage_scope.owned_smb_mount().is_some();
    for (path, expected) in source.subtree(relative) {
        progress.checkpoint()?;
        let suffix = path
            .strip_prefix(relative)
            .map_err(|_| stale(&source.root.join(path)))?;
        let target = if suffix.as_os_str().is_empty() {
            payload.clone()
        } else {
            payload.join(suffix)
        };
        let parent = bound
            .then(|| staged_parent(stage_scope, &target, &cancel))
            .transpose()?;
        let name = target.path().file_name().ok_or_else(|| stale(&target))?;
        match &expected.kind {
            Kind::Directory => match parent {
                Some(parent) => {
                    parent.exclusive_directory(name)?;
                }
                None => make_directory(&target)?,
            },
            Kind::File(_) => {
                let mut file = scope.require_file(path, &cancel)?;
                let (source, size, input) = file.stream();
                expected
                    .version
                    .verify_metadata(source, &input.metadata(source)?)?;
                let mut output = if let Some(parent) = parent {
                    let handle = parent.exclusive_file(name)?;
                    StagedFile::from_local(handle, &target, cancel.clone())
                } else {
                    StagedFile::open(&target, cancel.clone())?
                };
                let copied = progress.copy(
                    source.path(),
                    Some(size),
                    &mut AccessStream::new(input, source).take(size),
                    &mut AccessStream::new(&mut output, &target),
                )?;
                if copied != size {
                    return Err(CopyVerificationError::SourceSize {
                        source: source.clone(),
                        expected: size,
                        actual: copied,
                    }
                    .into());
                }
                if bound {
                    output.close_smb()?;
                } else {
                    output.close()?;
                }
                file.verify(&cancel)?;
            }
            Kind::Link(link) => {
                if let Some(parent) = parent {
                    parent.exclusive_link(name, Path::new(link))?;
                } else if let Some(path) = target.local() {
                    std::os::unix::fs::symlink(Path::new(link), path).map_err(|error| {
                        AccessError::new(&target, AccessAction::CreateLink, error)
                    })?;
                } else {
                    remote(&target)?
                        .symlink(Path::new(link), target.path())
                        .map_err(|error| {
                            AccessError::new(&target, AccessAction::CreateLink, error)
                        })?;
                }
            }
        }
    }
    Ok(())
}

fn staged_parent(
    scope: &ReadScope,
    target: &Location,
    cancel: &Cancellation,
) -> Result<Directory, TransferError> {
    let path = target
        .path()
        .strip_prefix(scope.parent.location.path())
        .ok()
        .and_then(Path::parent)
        .ok_or_else(|| stale(target))?;
    let location = scope.parent.location.join(path);
    Directory::at(scope, path, &read_metadata(&location)?, cancel)
}

fn preserve_attributes(
    source: &Snapshot,
    relative: &Path,
    staged: &Snapshot,
    incoming: &Path,
    scope: &ReadScope,
    progress: &mut Progress,
) -> Result<Option<MetadataWarning>, TransferError> {
    let mut warning = None;
    let cancel = progress.cancellation();
    // Apply directory permissions after their descendants have been prepared.
    for (path, target_entry) in staged.entries.iter().rev() {
        progress.checkpoint()?;
        let target = staged.root.join(path);
        let suffix = path.strip_prefix(incoming).map_err(|_| stale(&target))?;
        let source_path = relative.join(suffix);
        let source_entry =
            source
                .entries
                .get(&source_path)
                .ok_or_else(|| SyncError::VerificationMismatch {
                    check: SyncVerification::StagedCopy,
                    difference: SyncDifference::Unexpected,
                    original: source.root.join(&source_path),
                    inspected: target.clone(),
                })?;
        let attributes = copy_metadata::Attributes::from_entry(&source_entry.version.metadata);
        let issue = match source_entry.kind {
            Kind::Directory if scope.owned_smb_mount().is_some() => {
                let directory =
                    Directory::at(scope, path, &target_entry.version.metadata, &cancel)?;
                target_entry
                    .version
                    .verify_metadata(&target, &directory.current_metadata()?)?;
                let handle = directory.descriptor.ok_or_else(|| stale(&target))?;
                copy_metadata::finish_local_file(&target, &attributes, &handle.into())?
            }
            Kind::Directory => {
                copy_metadata::finish_directory(&target, &attributes, &target_entry.version)?
            }
            Kind::File(_) if scope.owned_smb_mount().is_some() => {
                let mut file = scope.require_file(path, &cancel)?;
                let (_, _, input) = file.stream();
                target_entry
                    .version
                    .verify_metadata(&target, &input.metadata(&target)?)?;
                copy_metadata::finish_file(&target, &attributes, input)?
            }
            Kind::File(_) => {
                let mut file = SourceReader::open(&target)?;
                target_entry
                    .version
                    .verify_metadata(&target, &file.metadata(&target)?)?;
                copy_metadata::finish_file(&target, &attributes, &mut file)?
            }
            Kind::Link(_) => None,
        };
        warning = warning.or(issue);
    }
    Ok(warning)
}

fn required_tree(
    scope: &ReadScope,
    path: &Path,
    exclusions: &Exclusions,
    progress: &mut Progress,
) -> Result<Snapshot, TransferError> {
    let cancel = progress.cancellation();
    cancel.check()?;
    scope.verify_root()?;
    let location = scope.parent.location.join(path);
    let relative_parent = path.parent().ok_or_else(|| stale(&location))?;
    let parent = scope.require_directory(relative_parent, &cancel)?;
    if canonical(&parent)? != parent.path() {
        return Err(stale(&parent));
    }
    let mut snapshot = Snapshot::empty(scope);
    let metadata = read_metadata(&location)?;
    if let Some(mount) = scope.owned_smb_mount()
        && scope.parent.same_local_volume(&metadata) != Some(true)
    {
        return Err(mount.changed(location.path()));
    }
    if metadata.is_directory() {
        snapshot.observe(
            scope,
            path.to_owned(),
            scan::Entry::Directory,
            exclusions,
            &mut Budget::default(),
            progress,
        )?;
    } else {
        let kind = if metadata.is_file() {
            Kind::File(
                scope
                    .require_file(path, &cancel)?
                    .digest(progress, &cancel)?,
            )
        } else if is_link(&metadata) {
            Kind::Link(read_link(&location)?.into_os_string())
        } else {
            return Err(stale(&location));
        };
        let version = TransferSourceEntry {
            metadata,
            children: None,
        };
        cancel.check()?;
        version.verify_metadata(&location, &read_metadata(&location)?)?;
        snapshot
            .entries
            .insert(path.to_owned(), Observed { kind, version });
    }
    scope.verify_root()?;
    Ok(snapshot)
}

#[derive(Clone, Copy)]
enum Check {
    Contents,
    Original,
    Held,
}

fn check_tree(
    expected: &Snapshot,
    root: &Path,
    actual: &Snapshot,
    actual_root: &Path,
    check: Check,
    cancel: &Cancellation,
) -> Result<(), TransferError> {
    let join_suffix = |path: &Path, suffix: &Path| {
        if suffix.as_os_str().is_empty() {
            path.to_owned()
        } else {
            path.join(suffix)
        }
    };
    let mismatch = |suffix: &Path, difference| {
        let inspected = actual.root.join(join_suffix(actual_root, suffix));
        let check = match check {
            Check::Contents => SyncVerification::StagedCopy,
            Check::Held => SyncVerification::HeldOriginal,
            Check::Original => {
                return match difference {
                    SyncDifference::Missing => SyncError::PlanEntryMissing(inspected).into(),
                    SyncDifference::Unexpected => SyncError::PlanEntryAdded(inspected).into(),
                    SyncDifference::Changed => stale(&inspected),
                };
            }
        };
        SyncError::VerificationMismatch {
            check,
            difference,
            original: expected.root.join(join_suffix(root, suffix)),
            inspected,
        }
        .into()
    };
    let mut actual_entries = actual.subtree(actual_root);
    for (path, entry) in expected.subtree(root) {
        cancel.check()?;
        let suffix = path
            .strip_prefix(root)
            .map_err(|_| stale(&expected.root.join(path)))?;
        let expected_path = join_suffix(actual_root, suffix);
        let location = actual.root.join(&expected_path);
        let Some((actual_path, current)) = actual_entries.next() else {
            return Err(mismatch(suffix, SyncDifference::Missing));
        };
        match actual_path.cmp(&expected_path) {
            Ordering::Greater => return Err(mismatch(suffix, SyncDifference::Missing)),
            Ordering::Less => {
                let extra = actual_path
                    .strip_prefix(actual_root)
                    .map_err(|_| stale(&actual.root.join(actual_path)))?;
                return Err(mismatch(extra, SyncDifference::Unexpected));
            }
            Ordering::Equal => {}
        }
        if entry.kind != current.kind {
            return Err(mismatch(suffix, SyncDifference::Changed));
        }
        let matches = match check {
            Check::Contents => true,
            // A rename can update ctime. Compare the held root's identity,
            // material contents and the attributes that should survive it.
            Check::Held if suffix.as_os_str().is_empty() => {
                entry
                    .version
                    .matches_moved_target(&current.version.metadata)
                    && match (&entry.version.metadata, &current.version.metadata) {
                        (EntryMetadata::Local(before), EntryMetadata::Local(after)) => {
                            before.permissions() == after.permissions()
                                && before.created().ok() == after.created().ok()
                        }
                        _ => true,
                    }
            }
            _ => {
                current.version.metadata.check_sftp_source(&location)?;
                entry
                    .version
                    .matches_metadata(&current.version.metadata)
                    .map_err(|cause| {
                        AccessError::new(&location, AccessAction::ReadMetadata, cause)
                    })?
            }
        };
        if !matches {
            return Err(mismatch(suffix, SyncDifference::Changed));
        }
    }
    if let Some((extra, _)) = actual_entries.next() {
        let suffix = extra
            .strip_prefix(actual_root)
            .map_err(|_| stale(&actual.root.join(extra)))?;
        return Err(mismatch(suffix, SyncDifference::Unexpected));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_sync_completion_keeps_both_warnings_on_the_first_history_row() {
        use crate::file_error::{AccessCause, MetadataField};
        use std::io;

        let temp = tempfile::tempdir().unwrap();
        let folders = ["from", "to"].map(|name| Location::Local(temp.path().join(name)));
        fs::create_dir_all(folders[0].join("group").path()).unwrap();
        fs::write(folders[0].join("group/child").path(), b"contents").unwrap();
        fs::create_dir(folders[1].path()).unwrap();
        let mut progress = Progress::default();
        let plan = SyncPlan::build(folders, false, Exclusions::default(), &mut progress).unwrap();
        let execution = Execution {
            folders: &plan.folders,
            roles: ONE_WAY_ROLES,
            observed: &plan.observed,
            rows: &plan.rows,
            exclusions: &plan.exclusions,
        };
        let groups = groups(execution.rows);
        let [group] = groups.as_slice() else {
            panic!("expected a single group");
        };
        assert_eq!(group.len(), 2);
        let target = execution.observed[1].root.join("group");
        let staging = execution.observed[1].root.join("temporary-data");
        let metadata_cause = io::Error::from_raw_os_error(libc::EIO);
        let metadata_message = metadata_cause.to_string();
        let metadata = MetadataWarning {
            fields: vec![MetadataField::Permissions, MetadataField::AccessTime],
            causes: vec![AccessCause::Local(metadata_cause)],
        };
        let cleanup_cause = io::Error::from_raw_os_error(libc::EACCES);
        let cleanup_message = cleanup_cause.to_string();
        let cleanup = StagingCleanupError::failed_cleanup(
            None,
            AccessError::new(
                &staging,
                AccessAction::RemoveStagingDirectory,
                cleanup_cause,
            )
            .into(),
            &staging,
            &target,
        );
        let metadata_text = crate::i18n::metadata_warning(&metadata);
        let cleanup_text = crate::i18n::file_error(&cleanup);
        assert!(metadata_text.contains(&metadata_message));
        assert!(cleanup_text.contains(&cleanup_message));
        assert!(cleanup_text.contains(&target.display()));
        assert!(cleanup_text.contains(&staging.display()));

        execution.plan_history(&mut progress);
        progress.results.begin(&execution.history_row(&group[0]).0);
        execution.record_completed_group(
            group,
            GroupWarnings {
                metadata: Some(metadata),
                cleanup: Some(cleanup),
            },
            &mut progress.results,
        );
        let history = progress
            .results
            .finish("sync".into(), 0, &Ok("complete".into()))
            .unwrap();
        let bytes = serde_json::to_vec(&history).unwrap();
        let restored: crate::result_history::Record = serde_json::from_slice(&bytes).unwrap();
        let [root, child] = restored.items.as_slice() else {
            panic!("expected two history rows");
        };
        assert_eq!(root.target.as_deref(), Some(target.display().as_str()));
        assert_eq!(root.outcome, Outcome::CompletedWithWarning);
        assert_eq!(
            root.reason,
            Some(format!("{metadata_text}\n{cleanup_text}"))
        );
        assert_eq!(
            child.target.as_deref(),
            Some(target.join("child").display().as_str())
        );
        assert_eq!(child.outcome, Outcome::Completed);
        assert!(child.reason.is_none());
    }

    #[test]
    fn staged_file_verification_displays_both_file_paths_verbatim() {
        let temp = tempfile::tempdir().unwrap();
        let scope = ReadScope::open(&Location::Local(temp.path().to_owned())).unwrap();
        let original = Path::new("original");
        let incoming = Path::new("incoming");
        let [source, staged] = [original, incoming].map(|path| scope.parent.location.join(path));
        fs::write(source.path(), b"from source").unwrap();
        fs::write(staged.path(), b"bad payload").unwrap();
        let mut progress = Progress::default();
        let exclusions = Exclusions::default();
        let expected = required_tree(&scope, original, &exclusions, &mut progress).unwrap();
        let actual = required_tree(&scope, incoming, &exclusions, &mut progress).unwrap();
        let error = check_tree(
            &expected,
            original,
            &actual,
            incoming,
            Check::Contents,
            &Cancellation::default(),
        )
        .unwrap_err();
        let TransferError::Failed(crate::file_error::Failure::Sync(error)) = error else {
            panic!("expected a sync verification error");
        };
        let SyncError::VerificationMismatch {
            check: SyncVerification::StagedCopy,
            difference: SyncDifference::Changed,
            original,
            inspected,
        } = *error
        else {
            panic!("expected both verification paths");
        };
        assert_eq!(original.display(), source.display());
        assert_eq!(inspected.display(), staged.display());
    }

    #[test]
    fn staged_verification_reports_the_unexpected_path_before_an_expected_sibling() {
        let temp = tempfile::tempdir().unwrap();
        let roots = ["source", "staging"].map(|name| temp.path().join(name));
        for (base, folder) in roots.iter().zip(["original", "incoming"]) {
            fs::create_dir_all(base.join(folder)).unwrap();
            fs::write(base.join(folder).join("z-expected"), b"contents").unwrap();
        }
        fs::write(roots[1].join("incoming/a-unexpected"), b"independent bytes").unwrap();
        let [source, staging] = roots.map(|path| ReadScope::open(&Location::Local(path)).unwrap());
        let original = Path::new("original");
        let incoming = Path::new("incoming");
        let mut progress = Progress::default();
        let exclusions = Exclusions::default();
        let expected = required_tree(&source, original, &exclusions, &mut progress).unwrap();
        let actual = required_tree(&staging, incoming, &exclusions, &mut progress).unwrap();
        let error = check_tree(
            &expected,
            original,
            &actual,
            incoming,
            Check::Contents,
            &Cancellation::default(),
        )
        .unwrap_err();
        assert!(
            matches!(error, TransferError::Failed(crate::file_error::Failure::Sync(error))
                if matches!(error.as_ref(), SyncError::VerificationMismatch {
                    check: SyncVerification::StagedCopy,
                    difference: SyncDifference::Unexpected,
                    original,
                    inspected,
                } if original == &expected.root.join("original/a-unexpected")
                    && inspected == &actual.root.join("incoming/a-unexpected"))
            )
        );
    }
}
