use super::{
    is_link, stale, Budget, Exclusions, Kind, Observed, Snapshot, SyncChange, SyncPlan, SyncRow,
};
use crate::{
    operations::{Cancellation, Progress, TransferError},
    result_history::Outcome,
    vfs::{
        compare::scan,
        copy_metadata, err,
        read_scope::{changed_during_read, local_subdirectory, ReadScope},
        sftp::remote,
        staged_file::StagedFile,
        transfer::{
            canonical, entry_metadata, rename_for_publication, with_preserved_staging,
            with_tracked_staging, EntryMetadata, LocalEntryMetadata, SourceReader, StagingNotice,
            TransferSourceEntry,
        },
        Location,
    },
};
use std::{
    ffi::OsStr,
    fs,
    io::{self, Read},
    os::{fd::OwnedFd, unix::fs::DirBuilderExt},
    path::Path,
};

impl SyncPlan {
    pub(crate) fn folders(&self) -> &[Location; 2] {
        &self.folders
    }

    pub(crate) fn apply(&self, progress: &mut Progress) -> Result<String, String> {
        let execution = Execution {
            folders: &self.folders,
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
) -> Result<String, String> {
    match result {
        Ok(()) => Ok(crate::tr_format!(
            "Синхронизация завершена. Выполнено: {completed}.",
            "Synchronization complete. Completed: {completed}."
        )),
        Err(TransferError::Cancelled) => {
            progress.results.cancelled = true;
            Ok(crate::tr_format!(
                "Синхронизация остановлена. Выполнено: {completed}.",
                "Synchronization stopped. Completed: {completed}."
            ))
        }
        Err(error) => Err(crate::tr_format!(
            "{error}. Выполнено: {completed}.",
            "{error}. Completed: {completed}."
        )),
    }
}

pub(super) struct Execution<'a> {
    pub(super) folders: &'a [Location; 2],
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
            for (input, scope) in self.folders.iter().zip(scopes) {
                scope.parent.verify()?;
                if canonical(input)? != scope.parent.location.path() {
                    return Err(stale(input));
                }
            }
            let source = 1 - row.destination;
            let destination = row.destination;
            progress.set_copy_endpoints(
                &self.observed[source].root,
                &self.observed[destination].root,
            );
            match self.apply_group(row, scopes, progress, staging.as_deref_mut()) {
                Ok(warning) => {
                    for (index, row) in group.iter().enumerate() {
                        let (key, target) = self.history_row(row);
                        let warning = (index == 0).then(|| warning.clone()).flatten();
                        let outcome = if warning.is_some() {
                            Outcome::CompletedWithWarning
                        } else {
                            Outcome::Completed
                        };
                        progress.results.record(&key, target, outcome, warning);
                    }
                    *completed += group.len();
                }
                Err(error @ TransferError::PublicationUnconfirmed(_)) => {
                    progress.results.record(
                        &key,
                        None,
                        Outcome::PublicationUnconfirmed,
                        Some(error.to_string()),
                    );
                    return Err(error);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn apply_group(
        &self,
        row: &SyncRow,
        scopes: &[ReadScope; 2],
        progress: &mut Progress,
        notice: Option<&mut StagingNotice<'_>>,
    ) -> Result<Option<String>, TransferError> {
        let source_scope = &scopes[1 - row.destination];
        let destination_scope = &scopes[row.destination];
        let source = &self.observed[1 - row.destination];
        let destination = &self.observed[row.destination];
        let relative = row.path.as_path();
        let parent_path = relative.parent().ok_or_else(changed_during_read)?;
        let name = relative.file_name().ok_or_else(changed_during_read)?;
        let target = destination.root.join(relative);
        let cancel = progress.cancellation();
        let parent_entry = destination
            .entries
            .get(parent_path)
            .ok_or_else(changed_during_read)?;
        let parent = Directory::at(
            destination_scope,
            parent_path,
            &parent_entry.version.metadata,
            &cancel,
        )?;
        let deletion_source = if row.change == SyncChange::Delete {
            let entry = source
                .entries
                .get(parent_path)
                .ok_or_else(changed_during_read)?;
            Some(Directory::at(
                source_scope,
                parent_path,
                &entry.version.metadata,
                &cancel,
            )?)
        } else {
            None
        };
        let mut committed = false;
        let mut metadata_warning = None;
        // Only a source subtree is filtered. A temporary copy and every
        // subtree being replaced or removed must match in full.
        let unfiltered = Exclusions::default();
        let work = |staging: &Location, preserve: &mut bool| {
            *preserve = false;
            parent.verify()?;
            let stage_scope = open_staging(staging)?;
            let stage = Directory::from_scope(&stage_scope)?;
            let incoming = Path::new("incoming");
            let payload = staging.join(incoming);
            if row.change != SyncChange::Delete {
                stage_source(source, relative, source_scope, &payload, progress)?;
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
                metadata_warning =
                    preserve_attributes(source, relative, &staged, incoming, progress)?;
            }

            if row.change == SyncChange::Copy {
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
                make_directory(&backup_folder)?;
                let backup_scope = open_staging(&backup_folder)?;
                let backup = Directory::from_scope(&backup_scope)?;
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
                    parent.verify()?;
                    backup.verify()?;
                    cancel.check()?;
                    if row.change == SyncChange::Replace {
                        stage.verify()?;
                        stage.rename(incoming.as_os_str(), &parent, name)?;
                    } else if let Some(source) = &deletion_source {
                        source.verify_absent(name)?;
                        parent.verify_absent(name)?;
                        if let Some(path) = backup.location.local() {
                            trash::delete(path.join(name)).map_err(err)?;
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
                        .and_then(|()| backup.rename(name, &parent, name));
                    if let Err(restore) = restore {
                        return Err(retained(
                            TransferError::PublicationUnconfirmed(crate::tr_format!(
                                "{error}. Прежний объект не удалось вернуть: {restore}",
                                "{error}. Could not restore the previous item: {restore}"
                            )),
                            staging,
                        ));
                    }
                    *preserve = false;
                    return Err(error);
                }
            }

            committed = true;
            *preserve = false;
            Ok(())
        };
        let result = match notice {
            Some(notice) => with_tracked_staging(&target, &cancel, notice, work),
            None => with_preserved_staging(&target, &cancel, work),
        };
        match result {
            Ok(()) => Ok(metadata_warning),
            Err(error) if committed => Ok(Some(match metadata_warning {
                Some(warning) => format!("{warning}. {error}"),
                None => error.to_string(),
            })),
            Err(error) => Err(error),
        }
    }
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

fn retained(error: TransferError, staging: &Location) -> TransferError {
    TransferError::PublicationUnconfirmed(crate::tr_format!(
        "{error}. Временная папка сохранена для проверки: {staging}.",
        "{error}. Temporary folder retained for inspection: {staging}."
    ))
}

fn make_directory(location: &Location) -> Result<(), TransferError> {
    if let Some(path) = location.local() {
        fs::DirBuilder::new().mode(0o700).create(path)?;
    } else {
        remote(location)?
            .mkdir(location.path(), 0o700)
            .map_err(err)?;
    }
    Ok(())
}

fn open_staging(location: &Location) -> Result<ReadScope, TransferError> {
    let scope = ReadScope::open(location)?;
    if scope.parent.location != *location {
        return Err(stale(location));
    }
    Ok(scope)
}

fn stage_source(
    source: &Snapshot,
    relative: &Path,
    scope: &ReadScope,
    payload: &Location,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let cancel = progress.cancellation();
    for (path, expected) in source.subtree(relative) {
        progress.checkpoint()?;
        let suffix = path.strip_prefix(relative).map_err(err)?;
        let target = if suffix.as_os_str().is_empty() {
            payload.clone()
        } else {
            payload.join(suffix)
        };
        match &expected.kind {
            Kind::Directory => make_directory(&target)?,
            Kind::File(_) => {
                scope
                    .read_checked(path, &cancel, |source, size, input| {
                        expected
                            .version
                            .verify_metadata(source, &input.metadata()?)?;
                        let mut output = StagedFile::open(&target, cancel.clone())?;
                        let copied = progress.copy(
                            source.path(),
                            Some(size),
                            &mut input.take(size),
                            &mut output,
                        )?;
                        if copied != size {
                            return Err(stale(source));
                        }
                        output.close()
                    })?
                    .ok_or_else(changed_during_read)?;
            }
            Kind::Link(link) => {
                if let Some(path) = target.local() {
                    std::os::unix::fs::symlink(Path::new(link), path)?;
                } else {
                    remote(&target)?
                        .symlink(Path::new(link), target.path())
                        .map_err(err)?;
                }
            }
        }
    }
    Ok(())
}

fn preserve_attributes(
    source: &Snapshot,
    relative: &Path,
    staged: &Snapshot,
    incoming: &Path,
    progress: &mut Progress,
) -> Result<Option<String>, TransferError> {
    let mut warning = None;
    // Apply directory permissions after their descendants have been prepared.
    for (path, target_entry) in staged.entries.iter().rev() {
        progress.checkpoint()?;
        let suffix = path.strip_prefix(incoming).map_err(err)?;
        let source_entry = source
            .entries
            .get(&relative.join(suffix))
            .ok_or_else(changed_during_read)?;
        let target = staged.root.join(path);
        let issue = match source_entry.kind {
            Kind::Directory => copy_metadata::finish_directory(
                &target,
                &source_entry.version.metadata,
                &target_entry.version,
            )?,
            Kind::File(_) => {
                let mut file = SourceReader::open(&target)?;
                target_entry
                    .version
                    .verify_metadata(&target, &file.metadata()?)?;
                copy_metadata::finish_file(&target, &source_entry.version.metadata, &mut file)?
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
    scope.parent.verify()?;
    let relative_parent = path.parent().ok_or_else(changed_during_read)?;
    let parent = scope
        .checked_directory(relative_parent, &cancel)?
        .ok_or_else(changed_during_read)?;
    if canonical(&parent)? != parent.path() {
        return Err(stale(&parent));
    }
    let mut snapshot = Snapshot::empty(scope);
    let location = snapshot.root.join(path);
    let metadata = entry_metadata(&location)?.ok_or_else(|| stale(&location))?;
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
                    .digest(path, progress, &cancel)?
                    .ok_or_else(changed_during_read)?,
            )
        } else if is_link(&metadata) {
            let target = if let Some(path) = location.local() {
                fs::read_link(path)?
            } else {
                remote(&location)?.readlink(location.path()).map_err(err)?
            };
            Kind::Link(target.into_os_string())
        } else {
            return Err(stale(&location));
        };
        let version = TransferSourceEntry {
            metadata,
            children: None,
        };
        version.verify(&location, &cancel, false)?;
        snapshot
            .entries
            .insert(path.to_owned(), Observed { kind, version });
    }
    scope.parent.verify()?;
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
    let mut actual_entries = actual.subtree(actual_root);
    for (path, entry) in expected.subtree(root) {
        cancel.check()?;
        let suffix = path.strip_prefix(root).map_err(err)?;
        let expected_path = actual_root.join(suffix);
        let location = actual.root.join(&expected_path);
        let (actual_path, current) = actual_entries.next().ok_or_else(|| stale(&location))?;
        if actual_path != &expected_path || entry.kind != current.kind {
            return Err(stale(&location));
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
            _ => entry
                .version
                .verify_metadata(&location, &current.version.metadata)
                .is_ok(),
        };
        if !matches {
            return Err(stale(&location));
        }
    }
    if let Some((extra, _)) = actual_entries.next() {
        return Err(stale(&actual.root.join(extra)));
    }
    Ok(())
}

struct Directory {
    location: Location,
    descriptor: Option<OwnedFd>,
    metadata: EntryMetadata,
}

impl Directory {
    fn from_scope(scope: &ReadScope) -> Result<Self, TransferError> {
        let location = scope.parent.location.clone();
        let metadata = entry_metadata(&location)?.ok_or_else(changed_during_read)?;
        Self::at(scope, Path::new(""), &metadata, &Cancellation::default())
    }

    fn at(
        scope: &ReadScope,
        relative: &Path,
        expected: &EntryMetadata,
        cancel: &Cancellation,
    ) -> Result<Self, TransferError> {
        scope.parent.verify()?;
        let location = scope
            .checked_directory(relative, cancel)?
            .ok_or_else(changed_during_read)?;
        let descriptor = scope
            .descriptor
            .as_ref()
            .map(|root| {
                local_subdirectory(root, relative, cancel)?
                    .map_or_else(|| rustix::io::dup(root).map_err(io::Error::from), Ok)
            })
            .transpose()?;
        let anchor = Self {
            location,
            descriptor,
            metadata: expected.clone(),
        };
        anchor.verify()?;
        if let Some(fd) = &anchor.descriptor {
            let file = fs::File::from(rustix::io::dup(fd).map_err(io::Error::from)?);
            let captured =
                EntryMetadata::Local(LocalEntryMetadata::from_file(&file, file.metadata()?));
            if !expected.same_object(&captured) {
                return Err(stale(&anchor.location));
            }
        }
        Ok(anchor)
    }

    fn verify(&self) -> Result<(), TransferError> {
        let actual = entry_metadata(&self.location)?.ok_or_else(|| stale(&self.location))?;
        if !actual.is_directory()
            || self.descriptor.is_some() && !self.metadata.same_object(&actual)
            || canonical(&self.location)? != self.location.path()
        {
            return Err(stale(&self.location));
        }
        Ok(())
    }

    fn verify_absent(&self, name: &OsStr) -> Result<(), TransferError> {
        self.verify()?;
        let path = self.location.join(name);
        if entry_metadata(&path)?.is_some() {
            return Err(stale(&path));
        }
        Ok(())
    }

    fn rename(
        &self,
        name: &OsStr,
        destination: &Directory,
        new_name: &OsStr,
    ) -> Result<(), TransferError> {
        let from = self.location.join(name);
        let to = destination.location.join(new_name);
        if let (Some(old_fd), Some(new_fd)) = (&self.descriptor, &destination.descriptor) {
            rustix::fs::renameat_with(
                old_fd,
                name,
                new_fd,
                new_name,
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(|error| {
                crate::fs_ops::rename_error(from.path(), to.path(), io::Error::from(error))
            })
            .map_err(Into::into)
        } else {
            rename_for_publication(&from, &to)
        }
    }
}
