use super::{
    changed_since_search, checked, folder_error, same_contents, ComparisonFailure, Member,
};
use crate::{
    file_error::{
        AccessAction, AccessError, DuplicateError, DuplicateRole, Failure, MacTrashError,
        MutationError, SmbMutationAction,
    },
    operations::{Cancellation, Progress, TransferError},
    result_history::Outcome,
    smb::MutationMount,
    vfs::{
        guarded_directory::{
            open_bound_staging, open_staging, retained, stale, uncertain_directory, Directory,
        },
        read_scope::ReadScope,
        transfer::{entry_metadata, with_preserved_empty_staging, EntryMetadata},
        undo::Parent,
        Location,
    },
};
use objc2::rc::autoreleasepool;
use objc2_foundation::{NSFileManager, NSURL};
use std::{ffi::CString, fs::File, io, os::unix::ffi::OsStrExt, path::Path, ptr::NonNull};

pub(crate) struct TrashPlan {
    pub(super) root: Parent,
    pub(super) mount: Option<MutationMount>,
    pub(super) groups: Vec<SelectedGroup>,
}

pub(super) struct SelectedGroup {
    pub(super) keeper: Member,
    pub(super) copies: Vec<Member>,
}

#[derive(Debug, Default)]
pub(crate) struct TrashReport {
    pub completed: usize,
    pub cleanup_warnings: usize,
    pub cancelled: bool,
}

impl TrashPlan {
    pub(crate) fn root(&self) -> &Path {
        self.root.location.path()
    }

    pub(crate) fn count(&self) -> usize {
        self.groups.iter().map(|group| group.copies.len()).sum()
    }

    pub(crate) fn apply(self, progress: &mut Progress) -> Result<TrashReport, TransferError> {
        self.apply_with(progress, system_trash)
    }

    fn open_scope(&self) -> Result<ReadScope, TransferError> {
        let location = &self.root.location;
        let mut mount = self
            .mount
            .clone()
            .unwrap_or_else(|| MutationMount::new(SmbMutationAction::Trash));
        mount.recheck()?;
        mount.check(location.path())?;
        self.root.verify()?;
        let scope = ReadScope::open_with_smb(location, &mut mount)?;
        if self.mount.is_none() && mount.is_owned() {
            return Err(mount.changed(location.path()));
        }
        self.root.verify()?;
        Directory::from_scope(&scope)?;
        Ok(scope)
    }

    fn apply_with(
        self,
        progress: &mut Progress,
        mut move_to_trash: impl FnMut(&Path, &Path) -> Result<(), TransferError>,
    ) -> Result<TrashReport, TransferError> {
        progress.results.plan_selected_items(
            self.groups
                .iter()
                .flat_map(|group| &group.copies)
                .map(|copy| self.root.location.join(&copy.path)),
        );
        let cancel = progress.cancellation();
        let mut report = TrashReport::default();
        let result = (|| {
            let root_error = |cause| folder_error(&self.root.location, cause);
            cancel.check()?;
            let scope = self.open_scope().map_err(root_error)?;
            let files = self
                .groups
                .iter()
                .flat_map(|group| group.copies.iter().map(|copy| (&group.keeper, copy)));
            for (keeper, copy) in files {
                progress.checkpoint()?;
                let location = scope.parent.location.join(&copy.path);
                progress.results.begin(&location);
                progress.item(location.path());
                let result = scope
                    .verify_mount()
                    .and_then(|()| self.root.verify())
                    .map_err(root_error)
                    .and_then(|()| trash_one(&scope, keeper, copy, &cancel, &mut move_to_trash));
                match result {
                    Ok(warning) => {
                        let outcome = if warning.is_some() {
                            report.cleanup_warnings += 1;
                            Outcome::CompletedWithWarning
                        } else {
                            Outcome::Completed
                        };
                        progress
                            .results
                            .record_with_error(&location, outcome, warning.as_ref());
                        report.completed += 1;
                    }
                    Err(error) => {
                        let cancelled = stopped(&error);
                        progress.results.cancelled |= cancelled;
                        let outcome = if matches!(error, TransferError::PublicationUnconfirmed(_)) {
                            Outcome::PublicationUnconfirmed
                        } else if cancelled {
                            Outcome::Cancelled
                        } else {
                            Outcome::Failed
                        };
                        progress
                            .results
                            .record_with_error(&location, outcome, Some(&error));
                        return Err(error);
                    }
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(report),
            Err(TransferError::Cancelled) => {
                progress.results.cancelled = true;
                report.cancelled = true;
                Ok(report)
            }
            Err(cause) => Err(DuplicateError::Batch {
                completed: report.completed,
                cleanup_warnings: report.cleanup_warnings,
                cause,
            }
            .into()),
        }
    }
}

fn system_trash(original: &Path, held: &Path) -> Result<(), TransferError> {
    let path = CString::new(held.as_os_str().as_bytes()).map_err(|source| {
        AccessError::new(
            &Location::Local(held.to_owned()),
            AccessAction::ResolvePath,
            io::Error::new(io::ErrorKind::InvalidInput, source),
        )
    })?;
    autoreleasepool(|_| {
        // SAFETY: The NUL-terminated filesystem representation outlives the call.
        // NSURL copies it. The selected payload was verified as a regular file.
        let url = unsafe {
            NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(
                NonNull::from(path.as_c_str()).cast(),
                false,
                None,
            )
        };
        NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, None)
            .map_err(|error| {
                DuplicateError::Trash {
                    location: Location::Local(original.to_owned()),
                    source: MacTrashError {
                        domain: error.domain().to_string(),
                        code: error.code(),
                        description: error.localizedDescription().to_string(),
                    },
                }
                .into()
            })
    })
}

fn stopped(error: &TransferError) -> bool {
    match error {
        TransferError::Cancelled => true,
        TransferError::Failed(Failure::StagingCleanup(error)) => {
            error.operation.as_ref().is_some_and(stopped)
        }
        TransferError::PublicationUnconfirmed(Failure::Duplicate(error)) => matches!(
            error.as_ref(),
            DuplicateError::Restore {
                cause: TransferError::Cancelled,
                ..
            }
        ),
        _ => false,
    }
}

fn trash_one(
    scope: &ReadScope,
    keeper: &Member,
    copy: &Member,
    cancel: &Cancellation,
    move_to_trash: &mut impl FnMut(&Path, &Path) -> Result<(), TransferError>,
) -> Result<Option<TransferError>, TransferError> {
    let location = scope.parent.location.join(&copy.path);
    let relative_parent = copy
        .path
        .parent()
        .ok_or_else(|| changed_since_search(&location))?;
    let name = copy
        .path
        .file_name()
        .ok_or_else(|| changed_since_search(&location))?;
    if keeper.path == copy.path || keeper.version.metadata.same_object(&copy.version.metadata) {
        return Err(changed_since_search(&location));
    }
    let parent_path = scope.parent.location.join(relative_parent);
    let metadata =
        entry_metadata(&parent_path)?.ok_or_else(|| changed_since_search(&parent_path))?;
    let parent = Directory::at(scope, relative_parent, &metadata, cancel)?;
    let protected = scope.parent.location.join(&keeper.path);
    let keeper_error =
        |cause| DuplicateError::verification(&protected, DuplicateRole::Protected, cause);
    let copy_error =
        |cause| DuplicateError::verification(&location, DuplicateRole::Selected, cause);
    let mount = scope.owned_smb_mount();
    let bound_parent = mount
        .map(|mount| {
            let handle = parent
                .descriptor
                .as_ref()
                .ok_or_else(|| stale(&parent_path))?;
            Ok::<_, TransferError>((handle, mount))
        })
        .transpose()?;
    parent.verify()?;
    let mut committed = false;
    let work = |staging: &Location, opened: Option<&File>, preserve: &mut bool| {
        *preserve = mount.is_some();
        let (backup_scope, backup) = (|| {
            let scope = if let Some(mount) = mount {
                let handle = opened.ok_or_else(|| stale(staging))?;
                open_bound_staging(staging, mount, handle)?
            } else {
                open_staging(staging)?
            };
            let directory = Directory::from_scope(&scope)?;
            Ok((scope, directory))
        })()
        .map_err(|cause| {
            if mount.is_some() {
                uncertain_directory(staging, cause)
            } else {
                cause
            }
        })?;
        *preserve = false;
        // Prevent a stale selection from even moving a different file. The
        // descriptor check on the held entry below closes the rename race.
        checked(scope, keeper, cancel)
            .and_then(|mut file| file.verify(cancel))
            .map_err(keeper_error)?;
        checked(scope, copy, cancel)
            .and_then(|mut file| file.verify(cancel))
            .map_err(copy_error)?;
        parent.verify()?;
        backup.verify()?;
        cancel.check()?;
        *preserve = true;
        match parent.rename(name, &backup, name) {
            Ok(()) => {}
            Err(error @ TransferError::PublicationUnconfirmed(_)) => {
                return Err(retained(error, staging));
            }
            Err(error) => {
                *preserve = false;
                return Err(error);
            }
        }

        let held = backup.location.join(name);
        let verification = (|| {
            let mut source = checked(scope, keeper, cancel).map_err(keeper_error)?;
            let mut duplicate = backup_scope
                .open_file(Path::new(name), cancel)
                .and_then(|file| file.ok_or_else(|| changed_since_search(&location)))
                .map_err(copy_error)?;
            let valid = copy
                .version
                .matches_moved_target(&duplicate.stamp().metadata)
                && match (&copy.version.metadata, &duplicate.stamp().metadata) {
                    (EntryMetadata::Local(before), EntryMetadata::Local(after)) => {
                        before.permissions() == after.permissions()
                            && before.created().ok() == after.created().ok()
                    }
                    _ => false,
                };
            if !valid {
                return Err(copy_error(changed_since_search(&location)));
            }
            if !same_contents(&mut source, &mut duplicate, cancel)
                .map_err(ComparisonFailure::cause)?
            {
                return Err(DuplicateError::DifferentContents {
                    selected: location.clone(),
                    protected: protected.clone(),
                }
                .into());
            }
            scope.verify_root()?;
            parent.verify_absent(name)?;
            backup.verify()?;
            source.verify(cancel).map_err(keeper_error)?;
            duplicate.verify(cancel).map_err(copy_error)?;
            move_to_trash(location.path(), held.path())
        })();
        if let Err(cause) = verification {
            let verify_identity = |path: &Location| -> Result<(), TransferError> {
                let actual = entry_metadata(path)?;
                if !actual.is_some_and(|actual| copy.version.metadata.same_object(&actual)) {
                    return Err(MutationError::PlanChanged(path.clone()).into());
                }
                Ok(())
            };
            let restore = (|| {
                backup.verify()?;
                parent.verify()?;
                verify_identity(&held)?;
                backup.rename(name, &parent, name)?;
                parent.verify()?;
                verify_identity(&location)
            })();
            if let Err(restore) = restore {
                return Err(DuplicateError::Restore {
                    original: location.clone(),
                    held,
                    cause,
                    restore,
                }
                .into());
            }
            *preserve = false;
            if matches!(cause, TransferError::Cancelled) {
                return Err(cause);
            }
            return Err(DuplicateError::Restored {
                location: location.clone(),
                cause,
            }
            .into());
        }
        committed = true;
        *preserve = false;
        Ok(())
    };
    let result = with_preserved_empty_staging(&location, cancel, bound_parent, work);
    match result {
        Ok(()) => Ok(None),
        Err(TransferError::Failed(Failure::StagingCleanup(error))) if committed => {
            Ok(Some(DuplicateError::CleanupAfterTrash(*error).into()))
        }
        Err(error) if committed => Ok(Some(error)),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::duplicates::find;
    use std::{collections::BTreeSet, fs, os::unix::fs::MetadataExt};

    fn duplicate_failure(error: &TransferError) -> &DuplicateError {
        match error {
            TransferError::Failed(Failure::Duplicate(reason))
            | TransferError::PublicationUnconfirmed(Failure::Duplicate(reason)) => reason,
            _ => panic!("expected a typed duplicate error: {error:?}"),
        }
    }

    fn trash_denied(original: &Path) -> TransferError {
        DuplicateError::Trash {
            location: Location::Local(original.to_owned()),
            source: MacTrashError {
                domain: "NSPOSIXErrorDomain".into(),
                code: 13,
                description: "fixture denied Trash".into(),
            },
        }
        .into()
    }

    fn assert_trash_denied(cause: &TransferError, original: &Path) {
        let DuplicateError::Trash { location, source } = duplicate_failure(cause) else {
            panic!("{cause:?}");
        };
        assert_eq!(location.path(), original);
        assert_eq!(source.domain, "NSPOSIXErrorDomain");
        assert_eq!(source.code, 13);
        assert_eq!(source.description, "fixture denied Trash");
    }

    fn fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        for name in ["first", "second"] {
            fs::write(temp.path().join(name), b"original bytes").unwrap();
        }
        temp
    }

    #[test]
    fn protects_the_chosen_original_and_moves_only_the_marked_copy() {
        let temp = fixture();
        let folder = &fs::canonicalize(temp.path()).unwrap();
        let results = find(
            &Location::Local(folder.to_path_buf()),
            &mut Progress::default(),
        )
        .unwrap();
        // The first file cannot be marked while protected, but it becomes
        // selectable when the user explicitly protects the other file.
        let selected = BTreeSet::from([(0, 0)]);
        assert!(results.trash_plan(&[0], &selected).is_none());
        let plan = results.trash_plan(&[1], &selected).unwrap();
        let inode = fs::metadata(folder.join("first")).unwrap().ino();
        let report = plan
            .apply_with(&mut Progress::default(), |original, held| {
                assert_eq!(original, folder.join("first"));
                assert_eq!(fs::metadata(held)?.ino(), inode);
                assert_eq!(held.file_name().unwrap(), "first");
                fs::rename(held, folder.join("temporary-trash"))?;
                Ok(())
            })
            .unwrap();
        assert_eq!(report.completed, 1);
        assert_eq!(report.cleanup_warnings, 0);
        assert!(!folder.join("first").exists());
        assert_eq!(fs::read(folder.join("second")).unwrap(), b"original bytes");
        assert_eq!(
            fs::read(folder.join("temporary-trash")).unwrap(),
            b"original bytes"
        );
        assert_eq!(fs::read_dir(folder).unwrap().count(), 2);
    }

    #[test]
    fn stale_keeper_refuses_the_operation_even_when_size_and_mtime_match() {
        let temp = fixture();
        let folder = &fs::canonicalize(temp.path()).unwrap();
        let plan = find(
            &Location::Local(folder.to_path_buf()),
            &mut Progress::default(),
        )
        .unwrap()
        .trash_plan(&[0], &BTreeSet::from([(0, 1)]))
        .unwrap();
        let source = folder.join("first");
        let before = fs::metadata(&source).unwrap();
        fs::write(&source, b"modified bytes").unwrap();
        fs::File::open(&source)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(before.modified().unwrap()))
            .unwrap();
        let after = fs::metadata(&source).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        let error = plan
            .apply_with(&mut Progress::default(), |_, _| {
                panic!("stale keeper reached Trash")
            })
            .unwrap_err();
        let DuplicateError::Batch {
            completed: 0,
            cause,
            ..
        } = duplicate_failure(&error)
        else {
            panic!("{error:?}");
        };
        let DuplicateError::Verify {
            role: DuplicateRole::Protected,
            location,
            cause,
        } = duplicate_failure(cause)
        else {
            panic!("{cause:?}")
        };
        assert_eq!(location.path(), source);
        assert!(matches!(duplicate_failure(cause), DuplicateError::Changed {
            cause: Some(TransferError::Failed(Failure::Mutation(error))), ..
        } if matches!(error.as_ref(), MutationError::SourceChanged(path) if path.path() == source)));
        assert_eq!(fs::read(folder.join("second")).unwrap(), b"original bytes");
        assert_eq!(fs::read_dir(folder).unwrap().count(), 2);
    }

    #[test]
    fn failure_and_cancellation_after_holding_restore_the_copy() {
        for cancel in [false, true] {
            let temp = fixture();
            let folder = &fs::canonicalize(temp.path()).unwrap();
            let plan = find(
                &Location::Local(folder.to_path_buf()),
                &mut Progress::default(),
            )
            .unwrap()
            .trash_plan(&[0], &BTreeSet::from([(0, 1)]))
            .unwrap();
            let copy = folder.join("second");
            let inode = fs::metadata(&copy).unwrap().ino();
            let mut progress = Progress::default();
            if cancel {
                let path = copy.clone();
                progress
                    .cancellation()
                    .set_checkpoint(move || !path.exists());
            }
            let mut attempts = 0;
            let result = plan.apply_with(&mut progress, |original, held| {
                attempts += 1;
                assert!(!copy.exists());
                assert_eq!(original, copy);
                assert_eq!(fs::metadata(held)?.ino(), inode);
                Err(trash_denied(original))
            });
            assert_eq!(attempts, usize::from(!cancel));
            assert_eq!(result.is_ok(), cancel);
            assert_eq!(progress.results.cancelled, cancel);
            assert_eq!(
                progress.results.items[0].outcome,
                if cancel {
                    Outcome::Cancelled
                } else {
                    Outcome::Failed
                }
            );
            assert_eq!(fs::metadata(&copy).unwrap().ino(), inode);
            assert_eq!(fs::read(&copy).unwrap(), b"original bytes");
            assert_eq!(fs::read(folder.join("first")).unwrap(), b"original bytes");
            assert_eq!(fs::read_dir(folder).unwrap().count(), 2);
        }
    }

    #[test]
    fn partial_trash_preserves_completed_warnings_restores_failure_and_leaves_later_items() {
        let temp = fixture();
        let root = &fs::canonicalize(temp.path()).unwrap();
        let third = root.join("third");
        let later = root.join("z-later");
        for path in [&third, &later] {
            fs::write(path, b"original bytes").unwrap();
        }
        let plan = find(&Location::Local(root.into()), &mut Progress::default())
            .unwrap()
            .trash_plan(&[0], &BTreeSet::from([(0, 1), (0, 2), (0, 3)]))
            .unwrap();
        let acknowledged = root.join("fixture-trash");
        let third_inode = fs::metadata(&third).unwrap().ino();
        let mut extra = None;
        let mut progress = Progress::default();
        let error = plan
            .apply_with(&mut progress, |original, held| {
                if original == root.join("second") {
                    let incidental = held.with_file_name("independent.txt");
                    fs::write(&incidental, b"do not remove")?;
                    extra = Some(incidental);
                    fs::rename(held, &acknowledged)?;
                    return Ok(());
                }
                assert_eq!(original, third, "a later file must stay unstarted");
                Err(trash_denied(original))
            })
            .unwrap_err();
        let DuplicateError::Batch {
            completed: 1,
            cleanup_warnings: 1,
            cause,
        } = duplicate_failure(&error)
        else {
            panic!("{error:?}")
        };
        let DuplicateError::Restored { location, cause } = duplicate_failure(cause) else {
            panic!("{cause:?}");
        };
        assert_eq!(location.path(), third);
        assert_trash_denied(cause, &third);
        assert_eq!(
            progress
                .results
                .items
                .iter()
                .map(|row| row.outcome)
                .collect::<Vec<_>>(),
            [
                Outcome::CompletedWithWarning,
                Outcome::Failed,
                Outcome::NotStarted
            ]
        );
        let extra = extra.unwrap();
        let warning = progress.results.items[0].reason.as_deref().unwrap();
        assert!(warning.contains(extra.parent().unwrap().to_str().unwrap()));
        assert!(warning.contains("Trash"));
        assert_eq!(fs::read(extra).unwrap(), b"do not remove");
        assert_eq!(fs::read(acknowledged).unwrap(), b"original bytes");
        assert_eq!(fs::metadata(&third).unwrap().ino(), third_inode);
        assert_eq!(fs::read(third).unwrap(), b"original bytes");
        assert_eq!(fs::read(later).unwrap(), b"original bytes");
        assert_eq!(fs::read(root.join("first")).unwrap(), b"original bytes");
    }

    #[test]
    fn unconfirmed_restore_keeps_both_causes_and_preserves_independent_entries() {
        for replace_held in [false, true] {
            let temp = fixture();
            let root = &fs::canonicalize(temp.path()).unwrap();
            let copy = root.join("second");
            let preserved = root.join("externally-moved");
            let inode = fs::metadata(&copy).unwrap().ino();
            let plan = find(&Location::Local(root.into()), &mut Progress::default())
                .unwrap()
                .trash_plan(&[0], &BTreeSet::from([(0, 1)]))
                .unwrap();
            let mut progress = Progress::default();
            let error = plan
                .apply_with(&mut progress, |original, held| {
                    if replace_held {
                        fs::rename(held, &preserved)?;
                        fs::write(held, b"unrelated held file")?;
                    } else {
                        fs::write(original, b"unrelated original name")?;
                    }
                    Err(trash_denied(original))
                })
                .unwrap_err();
            assert!(matches!(error, TransferError::PublicationUnconfirmed(_)));
            let DuplicateError::Batch {
                completed: 0,
                cause,
                ..
            } = duplicate_failure(&error)
            else {
                panic!("{error:?}")
            };
            let DuplicateError::Restore {
                original,
                held,
                cause,
                restore,
            } = duplicate_failure(cause)
            else {
                panic!("{cause:?}")
            };
            assert_eq!(original.path(), copy);
            assert_trash_denied(cause, &copy);
            assert_eq!(
                progress.results.items[0].outcome,
                Outcome::PublicationUnconfirmed
            );
            let reason = progress.results.items[0].reason.as_deref().unwrap();
            assert!(reason.contains(held.path().to_str().unwrap()));
            if replace_held {
                assert!(
                    matches!(restore, TransferError::Failed(Failure::Mutation(error))
                    if matches!(error.as_ref(), MutationError::PlanChanged(path) if path == held))
                );
                assert_eq!(fs::metadata(&preserved).unwrap().ino(), inode);
                assert_eq!(fs::read(held.path()).unwrap(), b"unrelated held file");
                assert!(!copy.exists());
            } else {
                assert!(
                    matches!(restore, TransferError::Failed(Failure::Mutation(error))
                    if matches!(error.as_ref(), MutationError::RenameLocal { source, .. }
                    if source.kind() == std::io::ErrorKind::AlreadyExists))
                );
                assert_eq!(fs::metadata(held.path()).unwrap().ino(), inode);
                assert_eq!(fs::read(&copy).unwrap(), b"unrelated original name");
            }
            assert_eq!(fs::read(root.join("first")).unwrap(), b"original bytes");
        }
    }
}
