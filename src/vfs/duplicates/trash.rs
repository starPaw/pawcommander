use super::{changed_since_search, checked, same_contents, Member};
use crate::{
    operations::{Cancellation, Progress, TransferError},
    result_history::Outcome,
    vfs::{
        guarded_directory::{open_staging, retained, Directory},
        read_scope::ReadScope,
        transfer::{entry_metadata, with_preserved_staging, EntryMetadata},
        undo::Parent,
    },
};
use std::path::Path;
use trash::macos::{DeleteMethod, TrashContextExtMacos};

pub(crate) struct TrashPlan {
    pub(super) root: Parent,
    pub(super) groups: Vec<SelectedGroup>,
}

pub(super) struct SelectedGroup {
    pub(super) keeper: Member,
    pub(super) copies: Vec<Member>,
}

impl TrashPlan {
    pub(crate) fn root(&self) -> &Path {
        self.root.location.path()
    }

    pub(crate) fn count(&self) -> usize {
        self.groups.iter().map(|group| group.copies.len()).sum()
    }

    pub(crate) fn apply(self, progress: &mut Progress) -> Result<String, String> {
        let mut trash = trash::TrashContext::default();
        trash.set_delete_method(DeleteMethod::NsFileManager);
        self.apply_with(progress, |path| {
            trash.delete(path).map_err(|error| error.to_string().into())
        })
    }

    fn apply_with(
        self,
        progress: &mut Progress,
        mut move_to_trash: impl FnMut(&Path) -> Result<(), TransferError>,
    ) -> Result<String, String> {
        progress.results.plan_selected_items(
            self.groups
                .iter()
                .flat_map(|group| &group.copies)
                .map(|copy| self.root.location.join(&copy.path)),
        );
        let cancel = progress.cancellation();
        let mut completed = 0;
        let mut cleanup_warnings = 0;
        let result = (|| {
            let verify_root = || {
                self.root
                    .verify()
                    .map_err(|_| changed_since_search(&self.root.location))
            };
            verify_root()?;
            let scope = ReadScope::open(&self.root.location)?;
            verify_root()?;
            Directory::from_scope(&scope)?;
            let files = self
                .groups
                .iter()
                .flat_map(|group| group.copies.iter().map(|copy| (&group.keeper, copy)));
            for (keeper, copy) in files {
                progress.checkpoint()?;
                let location = scope.parent.location.join(&copy.path);
                progress.results.begin(&location);
                progress.item(location.path());
                verify_root()?;
                let warning = trash_one(&scope, keeper, copy, &cancel, &mut move_to_trash)?;
                cleanup_warnings += usize::from(warning.is_some());
                progress
                    .results
                    .record(&location, None, Outcome::Completed, warning);
                completed += 1;
            }
            Ok(())
        })();
        match result {
            Ok(()) if cleanup_warnings > 0 => Ok(crate::tr_format!(
                "В корзину перемещено: {completed}. Предупреждения об очистке: {cleanup_warnings}.",
                "Moved to Trash: {completed}. Cleanup warnings: {cleanup_warnings}."
            )),
            Ok(()) => Ok(crate::tr_format!(
                "В корзину перемещено: {completed}.",
                "Moved to Trash: {completed}."
            )),
            Err(TransferError::Cancelled) => {
                progress.results.cancelled = true;
                Ok(crate::tr_format!(
                    "Операция остановлена. В корзину перемещено: {completed}.",
                    "Operation stopped. Moved to Trash: {completed}."
                ))
            }
            Err(error) => {
                let error = error.to_string();
                let reason = error.trim_end_matches('.');
                Err(crate::tr_format!(
                    "{reason}. В корзину перемещено: {completed}.",
                    "{reason}. Moved to Trash: {completed}."
                ))
            }
        }
    }
}

fn trash_one(
    scope: &ReadScope,
    keeper: &Member,
    copy: &Member,
    cancel: &Cancellation,
    move_to_trash: &mut impl FnMut(&Path) -> Result<(), TransferError>,
) -> Result<Option<String>, TransferError> {
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
    let mut committed = false;
    let result = with_preserved_staging(&location, cancel, |staging, preserve| {
        *preserve = false;
        let backup_scope = open_staging(staging)?;
        let backup = Directory::from_scope(&backup_scope)?;
        // Prevent a stale selection from even moving a different file. The
        // descriptor check on the held entry below closes the rename race.
        checked(scope, keeper, cancel)?.verify(cancel)?;
        checked(scope, copy, cancel)?.verify(cancel)?;
        parent.verify()?;
        backup.verify()?;
        cancel.check()?;
        *preserve = true;
        if let Err(error) = parent.rename(name, &backup, name) {
            *preserve = false;
            return Err(error);
        }

        let held = backup.location.join(name);
        let verification = (|| {
            let mut source = checked(scope, keeper, cancel)?;
            let mut duplicate = backup_scope
                .open_file(Path::new(name), cancel)?
                .ok_or_else(|| changed_since_search(&location))?;
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
            if !valid || !same_contents(&mut source, &mut duplicate, cancel)? {
                return Err(changed_since_search(&location));
            }
            scope.parent.verify()?;
            parent.verify_absent(name)?;
            backup.verify()?;
            source.verify(cancel)?;
            duplicate.verify(cancel)?;
            move_to_trash(held.path()).map_err(|error| {
                crate::tr_format!(
                    "Не удалось переместить «{}» в корзину: {error}",
                    "Could not move “{}” to Trash: {error}",
                    location.display()
                )
                .into()
            })
        })();
        if let Err(error) = verification {
            let restored = backup
                .verify()
                .and_then(|()| parent.verify())
                .and_then(|()| backup.rename(name, &parent, name));
            if let Err(restore_error) = restored {
                return Err(retained(
                    crate::tr_format!(
                        "{error}. Не удалось вернуть файл на прежнее место: {restore_error}",
                        "{error}. Could not restore the file to its original path: {restore_error}"
                    )
                    .into(),
                    staging,
                ));
            }
            *preserve = false;
            return Err(error);
        }
        committed = true;
        *preserve = false;
        Ok(())
    });
    match result {
        Ok(()) => Ok(None),
        Err(error) if committed => Ok(Some(error.to_string())),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{duplicates::find, Location};
    use std::{collections::BTreeSet, fs, os::unix::fs::MetadataExt};

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
        let folder = temp.path();
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
        plan.apply_with(&mut Progress::default(), |held| {
            assert_eq!(fs::metadata(held)?.ino(), inode);
            assert_eq!(held.file_name().unwrap(), "first");
            fs::rename(held, folder.join("temporary-trash"))?;
            Ok(())
        })
        .unwrap();
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
        let folder = temp.path();
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
        assert!(plan
            .apply_with(&mut Progress::default(), |_| panic!(
                "stale keeper reached Trash"
            ))
            .is_err());
        assert_eq!(fs::read(folder.join("second")).unwrap(), b"original bytes");
        assert_eq!(fs::read_dir(folder).unwrap().count(), 2);
    }

    #[test]
    fn failure_and_cancellation_after_holding_restore_the_copy() {
        for cancel in [false, true] {
            let temp = fixture();
            let folder = temp.path();
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
            let result = plan.apply_with(&mut progress, |held| {
                attempts += 1;
                assert!(!copy.exists());
                assert_eq!(fs::metadata(held)?.ino(), inode);
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into())
            });
            assert_eq!(attempts, usize::from(!cancel));
            assert_eq!(result.is_ok(), cancel);
            assert_eq!(progress.results.cancelled, cancel);
            assert_eq!(fs::metadata(&copy).unwrap().ino(), inode);
            assert_eq!(fs::read(&copy).unwrap(), b"original bytes");
            assert_eq!(fs::read(folder.join("first")).unwrap(), b"original bytes");
            assert_eq!(fs::read_dir(folder).unwrap().count(), 2);
        }
    }
}
