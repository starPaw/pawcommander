use super::*;
use crate::file_error::{Failure, UndoError};
use crate::result_history::Outcome;
use crate::vfs::transfer::entry_metadata;
use crate::vfs::{rename_with_undo, UndoBatch, UndoOutcome};

fn undo_error(error: &TransferError) -> &UndoError {
    match error {
        TransferError::Failed(Failure::Undo(error))
        | TransferError::PublicationUnconfirmed(Failure::Undo(error)) => error,
        other => panic!("Expected a typed undo error, got {other:?}"),
    }
}

fn run_move(sources: &[Location], target: &Location, decision: Decision, progress: &mut Progress) {
    transfer_with_conflicts(
        sources,
        target,
        TransferKind::Move,
        progress,
        |_| {
            Ok(Answer {
                decision,
                apply_to_group: false,
            })
        },
        |_, _| {},
    )
    .unwrap();
}

#[test]
fn undo_renames_in_reverse_order_and_never_replaces_an_occupied_name() {
    let root = tempfile::tempdir().unwrap();
    let a: Location = root.path().join("исходник.txt").into();
    fs::write(a.path(), b"original bytes").unwrap();
    let mut first = Progress::default();
    let b = rename_with_undo(&a, "second.txt", &mut first).unwrap();
    let mut second = Progress::default();
    let c = rename_with_undo(&b, "third.txt", &mut second).unwrap();
    assert_eq!(first.undo.len(), 1);
    assert_eq!(second.undo.len(), 1);
    fs::write(b.path(), b"external occupant").unwrap();
    let error = second.undo.restore(&mut Progress::default()).unwrap_err();
    let UndoError::Incomplete {
        restored: 0,
        remaining: 1,
        cause,
    } = undo_error(&error)
    else {
        panic!("Expected a failed undo with one entry left, got {error:?}");
    };
    assert!(
        matches!(undo_error(cause), UndoError::OriginalOccupied { target, source: None }
            if target.path() == b.path().canonicalize().unwrap())
    );
    assert_eq!(fs::read(b.path()).unwrap(), b"external occupant");
    assert_eq!(fs::read(c.path()).unwrap(), b"original bytes");
    assert_eq!(second.undo.len(), 1);
    fs::remove_file(b.path()).unwrap();
    second.undo.restore(&mut Progress::default()).unwrap();
    first.undo.restore(&mut Progress::default()).unwrap();
    assert_eq!(fs::read(a.path()).unwrap(), b"original bytes");
    assert!(!b.path().exists() && !c.path().exists());
}

#[test]
fn undo_refuses_replaced_objects_even_when_size_and_mtime_match() {
    let root = tempfile::tempdir().unwrap();
    let original: Location = root.path().join("original").into();
    fs::write(original.path(), b"same bytes").unwrap();
    let mut progress = Progress::default();
    let current = rename_with_undo(&original, "current", &mut progress).unwrap();
    let modified = fs::metadata(current.path()).unwrap().modified().unwrap();
    let moved_elsewhere = root.path().join("preserved-original");
    fs::rename(current.path(), &moved_elsewhere).unwrap();
    fs::write(current.path(), b"same bytes").unwrap();
    fs::File::options()
        .write(true)
        .open(current.path())
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let error = progress.undo.restore(&mut Progress::default()).unwrap_err();
    let UndoError::Incomplete { cause, .. } = undo_error(&error) else {
        panic!("Expected a changed item error, got {error:?}");
    };
    assert!(matches!(undo_error(cause), UndoError::ItemChanged(path)
        if path.path() == current.path().canonicalize().unwrap()));
    assert!(!original.path().exists());
    assert_eq!(fs::read(current.path()).unwrap(), b"same bytes");
    assert_eq!(fs::read(moved_elsewhere).unwrap(), b"same bytes");
}

#[cfg(unix)]
#[test]
fn undo_refuses_changed_permissions_until_restored() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let original: Location = root.path().join("original").into();
    fs::write(original.path(), b"unchanged").unwrap();
    fs::set_permissions(original.path(), fs::Permissions::from_mode(0o600)).unwrap();
    let mut progress = Progress::default();
    let current = rename_with_undo(&original, "current", &mut progress).unwrap();
    fs::set_permissions(current.path(), fs::Permissions::from_mode(0o644)).unwrap();

    let error = progress.undo.restore(&mut Progress::default()).unwrap_err();
    let UndoError::Incomplete { cause, .. } = undo_error(&error) else {
        panic!("Expected a changed item error, got {error:?}");
    };
    assert!(matches!(undo_error(cause), UndoError::ItemChanged(_)));
    assert!(!original.path().exists());
    assert_eq!(fs::read(current.path()).unwrap(), b"unchanged");

    fs::set_permissions(current.path(), fs::Permissions::from_mode(0o600)).unwrap();
    progress.undo.restore(&mut Progress::default()).unwrap();
    assert_eq!(fs::read(original.path()).unwrap(), b"unchanged");
    assert!(!current.path().exists());
}

#[test]
fn undo_move_keep_both_restores_original_name_and_keeps_existing_destination() {
    let root = tempfile::tempdir().unwrap();
    let target: Location = root.path().join("target").into();
    fs::create_dir(target.path()).unwrap();
    let source: Location = root.path().join("file.txt").into();
    fs::write(source.path(), b"moving").unwrap();
    fs::write(target.join("file.txt").path(), b"existing").unwrap();
    let mut moved = Progress::default();
    run_move(
        std::slice::from_ref(&source),
        &target,
        Decision::KeepBoth,
        &mut moved,
    );
    assert_eq!(moved.undo.len(), 1);
    moved.undo.restore(&mut Progress::default()).unwrap();
    assert_eq!(fs::read(source.path()).unwrap(), b"moving");
    assert_eq!(
        fs::read(target.join("file.txt").path()).unwrap(),
        b"existing"
    );
    assert!(!target.join("file (2).txt").path().exists());
    let mut replaced = Progress::default();
    run_move(&[source], &target, Decision::Replace, &mut replaced);
    assert_eq!(
        replaced.undo.len(),
        0,
        "replacement cannot restore the discarded old version"
    );
}

#[test]
fn undo_checks_descendants_and_original_parent_identity() {
    for change in ["child", "new-child", "parent"] {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("source");
        let target: Location = root.path().join("target").into();
        fs::create_dir_all(parent.join("tree/nested")).unwrap();
        fs::create_dir(target.path()).unwrap();
        fs::write(parent.join("tree/nested/file"), b"original").unwrap();
        let source: Location = parent.join("tree").into();
        let mut progress = Progress::default();
        run_move(&[source], &target, Decision::KeepBoth, &mut progress);
        assert_eq!(progress.undo.len(), 1);
        match change {
            "child" => {
                fs::write(target.join("tree/nested/file").path(), b"changed contents").unwrap()
            }
            "new-child" => fs::write(target.join("tree/new").path(), b"new").unwrap(),
            _ => {
                fs::rename(&parent, root.path().join("old-parent")).unwrap();
                fs::create_dir(&parent).unwrap();
            }
        }
        assert!(
            progress.undo.restore(&mut Progress::default()).is_err(),
            "{change}"
        );
        assert!(target.join("tree/nested/file").path().exists());
        assert!(!parent.join("tree").exists());
    }
}

#[test]
fn undo_partial_stop_keeps_only_unrestored_roots_and_records_descendants() {
    let root = tempfile::tempdir().unwrap();
    let target: Location = root.path().join("target").into();
    fs::create_dir(target.path()).unwrap();
    for name in ["a", "b"] {
        fs::create_dir(root.path().join(name)).unwrap();
        fs::write(root.path().join(name).join("child"), name).unwrap();
    }
    let sources = [root.path().join("a").into(), root.path().join("b").into()];
    let mut moved = Progress::default();
    run_move(&sources, &target, Decision::KeepBoth, &mut moved);
    let restored_b = root.path().join("b");
    let cancel = Cancellation::at_checkpoint(move || restored_b.exists());
    let mut partial = Progress::with_cancellation(cancel);
    assert_eq!(
        moved.undo.restore(&mut partial).unwrap(),
        UndoOutcome::Stopped {
            restored: 1,
            remaining: 1
        }
    );
    assert!(partial.results.cancelled);
    assert_eq!(moved.undo.len(), 1);
    assert_eq!(
        partial
            .results
            .items
            .iter()
            .filter(|row| row.outcome == Outcome::Completed)
            .count(),
        2
    );
    fs::write(root.path().join("b/child"), b"edited after undo").unwrap();
    moved.undo.restore(&mut Progress::default()).unwrap();
    assert_eq!(fs::read(root.path().join("a/child")).unwrap(), b"a");
    assert_eq!(
        fs::read(root.path().join("b/child")).unwrap(),
        b"edited after undo"
    );
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
}

#[test]
fn undo_final_rename_refuses_a_target_created_after_preflight() {
    let root = tempfile::tempdir().unwrap();
    let original: Location = root.path().join("original").into();
    fs::write(original.path(), b"original bytes").unwrap();
    let mut progress = Progress::default();
    let current = rename_with_undo(&original, "current", &mut progress).unwrap();
    let conflict = original.clone();
    let mut checks = 0;
    let cancel = Cancellation::at_checkpoint(move || {
        checks += 1;
        if checks == 2 {
            fs::write(conflict.path(), b"new occupant").unwrap();
        }
        false
    });
    let error = progress
        .undo
        .restore(&mut Progress::with_cancellation(cancel))
        .unwrap_err();
    let UndoError::Incomplete {
        restored: 0,
        remaining: 1,
        cause,
    } = undo_error(&error)
    else {
        panic!("Expected an occupied original name error, got {error:?}");
    };
    assert!(matches!(
        undo_error(cause),
        UndoError::OriginalOccupied { target, source: Some(source) }
            if target.path() == original.path().canonicalize().unwrap()
                && source.kind() == std::io::ErrorKind::AlreadyExists
    ));
    assert_eq!(fs::read(original.path()).unwrap(), b"new occupant");
    assert_eq!(fs::read(current.path()).unwrap(), b"original bytes");
    assert_eq!(progress.undo.len(), 1);
}

#[test]
fn undo_retires_a_restored_root_when_post_rename_verification_is_interrupted() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().canonicalize().unwrap();
    let target: Location = folder.join("target").into();
    fs::create_dir(target.path()).unwrap();
    fs::write(folder.join("a"), b"a").unwrap();
    fs::create_dir(folder.join("b")).unwrap();
    fs::write(folder.join("b/child"), b"original child").unwrap();
    fs::write(folder.join("c"), b"c").unwrap();
    let sources = ["a", "b", "c"].map(|name| Location::Local(folder.join(name)));
    let child = sources[1].join("child");

    let mut moved = Progress::default();
    run_move(&sources, &target, Decision::KeepBoth, &mut moved);
    assert_eq!(moved.undo.len(), 3);
    let mut attempt = Progress::default();
    let error = moved
        .undo
        .restore_checking(&mut attempt, |path| {
            if path == &child {
                Err(TransferError::Cancelled)
            } else {
                entry_metadata(path)
            }
        })
        .unwrap_err();

    assert!(matches!(&error, TransferError::PublicationUnconfirmed(_)));
    let UndoError::Incomplete {
        restored: 1,
        remaining: 1,
        cause,
    } = undo_error(&error)
    else {
        panic!("Expected one confirmed restoration and one unchecked root, got {error:?}");
    };
    assert!(
        matches!(undo_error(cause), UndoError::Unconfirmed { from, to, cause: TransferError::Cancelled }
        if from == &target.join("b") && to == &sources[1])
    );
    assert!(!attempt.results.cancelled);
    let statuses = attempt
        .results
        .items
        .iter()
        .map(|item| item.outcome)
        .collect::<Vec<_>>();
    assert_eq!(
        statuses,
        [
            Outcome::Completed,
            Outcome::PublicationUnconfirmed,
            Outcome::NotStarted,
            Outcome::PublicationUnconfirmed,
        ]
    );
    assert_eq!(fs::read(child.path()).unwrap(), b"original child");
    assert_eq!(fs::read(sources[2].path()).unwrap(), b"c");
    assert_eq!(moved.undo.len(), 1);
    assert_eq!(moved.undo.weight(), 1);
    fs::write(child.path(), b"edited after restoration").unwrap();
    assert_eq!(
        moved.undo.restore(&mut Progress::default()).unwrap(),
        UndoOutcome::Restored(1)
    );
    assert_eq!(fs::read(sources[0].path()).unwrap(), b"a");
    assert_eq!(fs::read(child.path()).unwrap(), b"edited after restoration");
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
}

#[test]
fn undo_cancelled_move_offers_only_confirmed_roots() {
    let root = tempfile::tempdir().unwrap();
    let target: Location = root.path().join("target").into();
    fs::create_dir(target.path()).unwrap();
    for name in ["a", "b"] {
        fs::write(root.path().join(name), name).unwrap();
    }
    let sources = [root.path().join("a").into(), root.path().join("b").into()];
    let first = target.join("a");
    let mut progress =
        Progress::with_cancellation(Cancellation::at_checkpoint(move || first.path().exists()));
    run_move(&sources, &target, Decision::KeepBoth, &mut progress);
    assert_eq!(progress.undo.len(), 1);
    progress.undo.restore(&mut Progress::default()).unwrap();
    assert_eq!(fs::read(root.path().join("a")).unwrap(), b"a");
    assert_eq!(fs::read(root.path().join("b")).unwrap(), b"b");
    assert_eq!(UndoBatch::default().len(), 0);
}

pub(super) fn sftp_undo(root: &Location) {
    let source = create_directory(root, "undo-source").unwrap();
    let target = create_directory(root, "undo-target").unwrap();
    fs::write(source.join("old name").path(), b"remote undo").unwrap();
    let mut renamed = Progress::default();
    let current = rename_with_undo(&source.join("old name"), "new name", &mut renamed).unwrap();
    assert_eq!(renamed.undo.len(), 1);
    renamed.undo.restore(&mut Progress::default()).unwrap();
    assert!(!current.path().exists());
    fs::create_dir(source.join("tree").path()).unwrap();
    fs::write(source.join("tree/nested").path(), b"nested remote undo").unwrap();
    let mut moved = Progress::default();
    run_move(
        &[source.join("old name"), source.join("tree")],
        &target,
        Decision::KeepBoth,
        &mut moved,
    );
    assert_eq!(moved.undo.len(), 2);
    fs::write(source.join("tree").path(), b"external occupant").unwrap();
    assert!(moved.undo.restore(&mut Progress::default()).is_err());
    assert_eq!(
        fs::read(source.join("tree").path()).unwrap(),
        b"external occupant"
    );
    fs::remove_file(source.join("tree").path()).unwrap();
    moved.undo.restore(&mut Progress::default()).unwrap();
    assert_eq!(
        fs::read(source.join("old name").path()).unwrap(),
        b"remote undo"
    );
    assert_eq!(
        fs::read(source.join("tree/nested").path()).unwrap(),
        b"nested remote undo"
    );
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut uncertain = Progress::default();
        let moved =
            rename_with_undo(&source.join("old name"), "uncertain", &mut uncertain).unwrap();
        fs::set_permissions(source.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let mut attempt = Progress::default();
        let result = uncertain.undo.restore(&mut attempt);
        fs::set_permissions(source.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let error = result.unwrap_err();
        assert!(matches!(error, TransferError::PublicationUnconfirmed(_)));
        assert!(matches!(
            undo_error(&error),
            UndoError::Incomplete {
                restored: 0,
                remaining: 0,
                cause: TransferError::PublicationUnconfirmed(Failure::Mutation(_))
            }
        ));
        assert_eq!(
            uncertain.undo.len(),
            0,
            "an issued remote rename error is never replayed"
        );
        assert_eq!(
            attempt.results.items[0].outcome,
            Outcome::PublicationUnconfirmed
        );
        assert_eq!(fs::read(moved.path()).unwrap(), b"remote undo");
        assert!(!source.join("old name").path().exists());
    }
    fs::remove_dir_all(source.path()).unwrap();
    fs::remove_dir(target.path()).unwrap();
}
