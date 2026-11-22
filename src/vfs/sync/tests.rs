use super::*;
use crate::file_error::{AccessCause, Failure, MutationError, PublicationError, RecoveryNotice};
use std::{
    fs,
    os::unix::fs::symlink,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn folders() -> ([tempfile::TempDir; 2], [Location; 2]) {
    let temporary = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let locations = temporary
        .each_ref()
        .map(|folder| folder.path().to_owned().into());
    (temporary, locations)
}

fn rows(plan: &SyncPlan) -> BTreeMap<PathBuf, SyncChange> {
    plan.rows
        .iter()
        .map(|row| (row.path.clone(), row.change))
        .collect()
}

fn set_modified(path: &Path, time: SystemTime) {
    fs::File::open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(time))
        .unwrap();
}

fn folder_cause(error: TransferError, role: SyncFolderRole, folder: &Location) -> TransferError {
    let TransferError::Failed(Failure::Sync(error)) = error else {
        panic!("expected synchronization folder context, got {error:?}");
    };
    let SyncError::Folder {
        role: actual,
        folder: selected,
        cause,
    } = *error
    else {
        panic!("expected synchronization folder context, got {error:?}");
    };
    assert_eq!(actual, role);
    assert_eq!(&selected, folder);
    *cause
}

#[test]
fn plan_lists_the_exact_changes_including_a_replaced_directory() {
    let (_temporary, pair) = folders();
    let [source, destination] = &pair;
    fs::create_dir(source.join("new").path()).unwrap();
    fs::write(source.join("new/.hidden").path(), b"new").unwrap();
    fs::write(source.join("type").path(), b"source file").unwrap();
    fs::create_dir(destination.join("type").path()).unwrap();
    fs::write(destination.join("type/old").path(), b"old subtree").unwrap();
    fs::write(destination.join("extra").path(), b"keep by default").unwrap();
    fs::write(source.join("changed").path(), b"new!").unwrap();
    fs::write(destination.join("changed").path(), b"old!").unwrap();
    fs::write(source.join("same").path(), b"same").unwrap();
    fs::write(destination.join("same").path(), b"same").unwrap();
    let time = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    set_modified(source.join("changed").path(), time);
    set_modified(destination.join("changed").path(), time);
    set_modified(destination.join("same").path(), time);
    symlink("new/.hidden", source.join("link").path()).unwrap();
    symlink("missing", destination.join("link").path()).unwrap();

    let mut progress = Progress::default();
    let plan = SyncPlan::build(pair.clone(), false, Exclusions::default(), &mut progress).unwrap();
    assert_eq!(
        rows(&plan),
        [
            ("changed", SyncChange::Replace),
            ("link", SyncChange::Replace),
            ("new", SyncChange::Copy),
            ("new/.hidden", SyncChange::Copy),
            ("type", SyncChange::Replace),
            ("type/old", SyncChange::Delete),
        ]
        .map(|(path, change)| (PathBuf::from(path), change))
        .into()
    );
    assert_eq!((plan.copies, plan.replacements, plan.deletions), (2, 3, 1));
    plan.verify(&mut progress).unwrap();
    assert_eq!(
        fs::read(destination.join("type/old").path()).unwrap(),
        b"old subtree"
    );
    assert!(!destination.join("new").path().exists());
    assert_eq!(
        fs::read_link(destination.join("link").path()).unwrap(),
        Path::new("missing")
    );

    let mirror = SyncPlan::build(pair, true, Exclusions::default(), &mut progress).unwrap();
    assert_eq!(rows(&mirror)[Path::new("extra")], SyncChange::Delete);
    assert_eq!(mirror.deletions, 2);
}

#[test]
fn plan_rejects_nested_or_aliased_folders() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("folder");
    let nested = folder.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let alias = root.path().join("alias");
    symlink(&folder, &alias).unwrap();
    let error = SyncPlan::build(
        [alias.into(), nested.clone().into()],
        true,
        Exclusions::default(),
        &mut Progress::default(),
    )
    .err()
    .unwrap();
    let TransferError::Failed(Failure::Sync(error)) = error else {
        panic!("expected typed overlapping folders");
    };
    let SyncError::Overlap {
        source,
        destination,
    } = *error
    else {
        panic!("expected both overlapping paths");
    };
    assert_eq!(source.path(), folder.canonicalize().unwrap());
    assert_eq!(destination.path(), nested.canonicalize().unwrap());
}

#[test]
fn verification_rejects_new_items_even_when_they_were_not_in_the_plan() {
    let (_temporary, pair) = folders();
    let mut progress = Progress::default();
    let plan = SyncPlan::build(pair.clone(), false, Exclusions::default(), &mut progress).unwrap();
    fs::write(pair[1].join("new-extra").path(), b"unexpected").unwrap();
    assert!(plan.verify(&mut progress).is_err());
    assert_eq!(
        fs::read(pair[1].join("new-extra").path()).unwrap(),
        b"unexpected"
    );
}

#[test]
fn apply_publishes_verified_copies_and_both_type_replacements() {
    use crate::result_history::Outcome;

    let (_temporary, pair) = folders();
    let [source, target] = &pair;
    fs::create_dir_all(source.join("folder-over-file").path()).unwrap();
    fs::write(source.join("folder-over-file/.child").path(), b"child").unwrap();
    symlink(".child", source.join("folder-over-file/link").path()).unwrap();
    fs::write(target.join("folder-over-file").path(), b"old file").unwrap();
    fs::write(source.join("file-over-folder").path(), b"source file").unwrap();
    fs::create_dir_all(target.join("file-over-folder/old").path()).unwrap();
    fs::write(target.join("file-over-folder/old/data").path(), b"old tree").unwrap();
    fs::write(source.join("new").path(), b"copy").unwrap();
    fs::write(source.join("edit").path(), b"new!").unwrap();
    fs::write(target.join("edit").path(), b"old!").unwrap();
    fs::write(target.join("extra").path(), b"keep").unwrap();

    let mut progress = Progress::default();
    let plan = SyncPlan::build(pair.clone(), false, Exclusions::default(), &mut progress).unwrap();
    let row_count = plan.rows.len();
    let report = plan.apply(&mut progress);
    report.result.unwrap();
    assert_eq!(report.completed, row_count);
    for (path, bytes) in [
        ("folder-over-file/.child", b"child".as_slice()),
        ("file-over-folder", b"source file"),
        ("new", b"copy"),
        ("edit", b"new!"),
    ] {
        assert_eq!(fs::read(source.join(path).path()).unwrap(), bytes);
        assert_eq!(fs::read(target.join(path).path()).unwrap(), bytes);
    }
    assert_eq!(
        fs::read_link(target.join("folder-over-file/link").path()).unwrap(),
        Path::new(".child")
    );
    assert_eq!(fs::read(target.join("extra").path()).unwrap(), b"keep");
    assert!(fs::read_dir(target.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pawcommander-")
    }));
    let outcomes = progress.results.root_outcomes();
    assert_eq!(outcomes.len(), row_count);
    assert!(outcomes
        .iter()
        .all(|(_, status)| *status == Outcome::Completed));
    assert_eq!(
        outcomes
            .iter()
            .find(|(location, _)| location.path().ends_with("file-over-folder/old/data"))
            .unwrap()
            .1,
        Outcome::Completed
    );
}

#[test]
fn apply_revalidates_before_its_first_mutation() {
    use crate::result_history::Outcome;

    let (_temporary, pair) = folders();
    let [source, target] = &pair;
    fs::write(source.join("a-copy").path(), b"copy").unwrap();
    fs::write(source.join("z-replace").path(), b"want").unwrap();
    let item = target.join("z-replace");
    fs::write(item.path(), b"have").unwrap();
    let time = UNIX_EPOCH + Duration::from_secs(1_650_000_000);
    set_modified(item.path(), time);
    let mut progress = Progress::default();
    let plan = SyncPlan::build(pair.clone(), false, Exclusions::default(), &mut progress).unwrap();
    fs::write(item.path(), b"edit").unwrap();
    set_modified(item.path(), time);
    let report = plan.apply(&mut progress);
    assert_eq!(report.completed, 0);
    let cause = folder_cause(
        report.result.unwrap_err(),
        SyncFolderRole::Destination,
        target,
    );
    let TransferError::Failed(Failure::Mutation(error)) = cause else {
        panic!("expected a typed stale plan error");
    };
    assert!(matches!(*error, MutationError::PlanChanged(path)
        if path.path() == item.path().canonicalize().unwrap()));
    assert!(progress
        .results
        .root_outcomes()
        .iter()
        .all(|(_, status)| *status == Outcome::NotStarted));
    assert_eq!(fs::read(item.path()).unwrap(), b"edit");
    assert!(!target.join("a-copy").path().exists());
}

#[test]
fn checking_a_plan_identifies_the_changed_child_when_membership_differs() {
    for (remove, add) in [(true, false), (false, true), (true, true)] {
        let (_temporary, pair) = folders();
        let target = &pair[1];
        let branch = target.join("branch");
        fs::create_dir(branch.path()).unwrap();
        let old = branch.join("a-prior");
        let new = branch.join("z-added");
        fs::write(old.path(), b"prior bytes").unwrap();
        let plan = SyncPlan::build(
            pair.clone(),
            true,
            Exclusions::default(),
            &mut Progress::default(),
        )
        .unwrap();
        if remove {
            fs::remove_file(old.path()).unwrap();
        }
        if add {
            fs::write(new.path(), b"new bytes").unwrap();
        }
        let expected =
            branch
                .path()
                .canonicalize()
                .unwrap()
                .join(if remove { "a-prior" } else { "z-added" });
        let cause = folder_cause(
            plan.verify(&mut Progress::default()).unwrap_err(),
            SyncFolderRole::Destination,
            target,
        );
        let TransferError::Failed(Failure::Sync(error)) = cause else {
            panic!("expected a change to plan membership");
        };
        match *error {
            SyncError::PlanEntryMissing(location) => assert!(remove && location.path() == expected),
            SyncError::PlanEntryAdded(location) => assert!(!remove && location.path() == expected),
            error => panic!("unexpected membership change {error:?}"),
        }
        if add {
            assert_eq!(fs::read(new.path()).unwrap(), b"new bytes");
        }
    }
}

#[test]
fn snapshot_reports_disappearance_type_changes_and_retargeted_links() {
    for scenario in ["missing", "type", "link"] {
        let (temporary, pair) = folders();
        let location = pair[0].join("observed");
        if scenario == "link" {
            symlink("first-target", location.path()).unwrap();
        } else {
            fs::write(location.path(), b"original bytes").unwrap();
        }
        let scope = ReadScope::open(&pair[0]).unwrap();
        let cancel = Cancellation::default();
        let mut observed = None;
        scan::visit(&scope, Path::new(""), &cancel, &mut |name, entry| {
            assert_eq!(name, "observed");
            observed = Some(entry);
            Ok(())
        })
        .unwrap();
        let held = temporary[1].path().join("preserved");
        fs::rename(location.path(), &held).unwrap();
        match scenario {
            "type" => fs::create_dir(location.path()).unwrap(),
            "link" => symlink("other-target", location.path()).unwrap(),
            _ => {}
        }
        let mut snapshot = Snapshot::empty(&scope);
        let cause = snapshot
            .observe(
                &scope,
                "observed".into(),
                observed.unwrap(),
                &Exclusions::default(),
                &mut Budget::default(),
                &mut Progress::default(),
            )
            .unwrap_err();
        let expected = scope.parent.location.join("observed");
        match scenario {
            "missing" => assert!(
                matches!(cause, TransferError::Failed(Failure::Access(error))
                    if error.location == expected && error.action == AccessAction::ReadMetadata
                        && matches!(&error.cause, AccessCause::Local(source)
                            if source.raw_os_error() == Some(libc::ENOENT))
                )
            ),
            "type" => assert!(matches!(cause, TransferError::Failed(Failure::Sync(error))
                if matches!(error.as_ref(), SyncError::EntryTypeChanged(path) if path == &expected)
            )),
            _ => assert!(matches!(cause, TransferError::Failed(Failure::Sync(error))
                if matches!(error.as_ref(), SyncError::LinkTargetChanged(path) if path == &expected)
            )),
        }
        assert!(snapshot.entries.is_empty());
        if scenario == "link" {
            assert_eq!(fs::read_link(&held).unwrap(), Path::new("first-target"));
            assert_eq!(
                fs::read_link(location.path()).unwrap(),
                Path::new("other-target")
            );
        } else {
            assert_eq!(fs::read(&held).unwrap(), b"original bytes");
        }
    }
}

#[test]
fn apply_reports_a_missing_source_after_a_confirmed_copy() {
    use crate::result_history::Outcome;

    let (_temporary, pair) = folders();
    let [source, destination] = &pair;
    let names = ["a-copy", "b-missing", "z-pending"];
    for name in names {
        fs::write(source.join(name).path(), name).unwrap();
    }
    let plan = SyncPlan::build(
        pair.clone(),
        false,
        Exclusions::default(),
        &mut Progress::default(),
    )
    .unwrap();
    let holding = tempfile::tempdir().unwrap();
    let held = holding.path().join("preserved");
    let missing = source.join("b-missing");
    let committed = destination.join("a-copy");
    let old_path = missing.path().to_owned();
    let held_path = held.clone();
    let mut moved = false;
    let cancel = Cancellation::at_checkpoint(move || {
        if !moved && committed.path().is_file() {
            fs::rename(&old_path, &held_path).unwrap();
            moved = true;
        }
        false
    });
    let mut progress = Progress::with_cancellation(cancel);
    let report = plan.apply(&mut progress);
    assert_eq!(report.completed, 1);
    let Err(TransferError::Failed(Failure::Access(error))) = report.result else {
        panic!("expected the missing source's original access failure");
    };
    let source_root = fs::canonicalize(source.path()).unwrap();
    assert_eq!(error.location.path(), source_root.join("b-missing"));
    assert_eq!(error.action, AccessAction::ReadMetadata);
    assert!(matches!(&error.cause, AccessCause::Local(cause)
        if cause.raw_os_error() == Some(libc::ENOENT)
    ));
    let rows = &progress.results.items;
    assert_eq!(
        rows.iter().map(|row| row.outcome).collect::<Vec<_>>(),
        [Outcome::Completed, Outcome::Failed, Outcome::NotStarted]
    );
    let reason = rows[1].reason.as_deref().unwrap();
    assert!(reason.contains(&error.location.display()) && reason.contains("(os error 2)"));
    assert_eq!(fs::read(held).unwrap(), b"b-missing");
    assert_eq!(
        fs::read(destination.join("a-copy").path()).unwrap(),
        b"a-copy"
    );
    assert!(!destination.join("b-missing").path().exists());
    assert!(!destination.join("z-pending").path().exists());
}

#[test]
fn failed_replacement_and_restore_keep_both_causes_and_versions() {
    use crate::result_history::Outcome;

    let (_temporary, pair) = folders();
    let [source, target] = &pair;
    fs::write(source.join("a-copy").path(), b"confirmed").unwrap();
    fs::write(source.join("z-replace").path(), b"incoming").unwrap();
    fs::write(target.join("z-replace").path(), b"original").unwrap();
    let destination = target.path().canonicalize().unwrap();
    let observer = destination.clone();
    let mut arrived = false;
    let cancel = Cancellation::at_checkpoint(move || {
        if !arrived
            && fs::read_dir(&observer).unwrap().any(|entry| {
                let entry = entry.unwrap();
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pawcommander-transfer-")
                    && entry.path().join("previous/z-replace").is_file()
            })
        {
            let mut item = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(observer.join("z-replace"))
                .unwrap();
            std::io::Write::write_all(&mut item, b"arrived").unwrap();
            arrived = true;
        }
        false
    });
    let plan = SyncPlan::build(
        pair.clone(),
        false,
        Exclusions::default(),
        &mut Progress::default(),
    )
    .unwrap();
    let mut progress = Progress::with_cancellation(cancel);
    let report = plan.apply(&mut progress);
    assert_eq!(report.completed, 1);
    let summary = SyncError::applying(report.completed, report.result.unwrap_err());
    let TransferError::PublicationUnconfirmed(Failure::Sync(summary)) = summary else {
        panic!("the job summary must preserve unconfirmed publication");
    };
    let SyncError::Apply {
        completed: 1,
        cause,
    } = *summary
    else {
        panic!("expected the number of confirmed changes");
    };
    let TransferError::PublicationUnconfirmed(Failure::Publication(error)) = *cause else {
        panic!("expected an unconfirmed publication");
    };
    let PublicationError::Retained {
        staging,
        notice: RecoveryNotice::InspectStaging,
        cause,
    } = *error
    else {
        panic!("expected the retained staging location");
    };
    let TransferError::Failed(Failure::Publication(error)) = cause else {
        panic!("expected a typed failed restore");
    };
    let PublicationError::Rollback {
        target,
        backup,
        cause,
        restore,
    } = *error
    else {
        panic!("expected both failures and both paths");
    };
    assert_eq!(target.path(), destination.join("z-replace"));
    assert_eq!(backup, staging.join("previous/z-replace"));
    let incoming = staging.join("incoming");
    for (error, from) in [(cause, &incoming), (restore, &backup)] {
        let TransferError::Failed(Failure::Mutation(rename)) = error else {
            panic!("expected an actual rename failure");
        };
        assert!(
            matches!(*rename, MutationError::RenameLocal { from: attempted, to, source }
            if attempted == from.path() && to == target.path() && source.raw_os_error() == Some(libc::EEXIST))
        );
    }
    assert_eq!(fs::read(target.path()).unwrap(), b"arrived");
    assert_eq!(fs::read(backup.path()).unwrap(), b"original");
    assert_eq!(fs::read(incoming.path()).unwrap(), b"incoming");
    assert_eq!(
        fs::read(source.join("z-replace").path()).unwrap(),
        b"incoming"
    );
    assert_eq!(fs::read(destination.join("a-copy")).unwrap(), b"confirmed");
    assert_eq!(progress.results.items[0].outcome, Outcome::Completed);
    assert_eq!(
        progress.results.items[1].outcome,
        Outcome::PublicationUnconfirmed
    );
}

#[test]
fn profile_exclusions_filter_new_subtrees_and_protect_items_on_both_sides() {
    let (_temporary, [left, right]) = folders();
    fs::create_dir_all(left.join("new/cache").path()).unwrap();
    fs::write(left.join("new/cache/value").path(), b"cache").unwrap();
    fs::write(left.join("new/ignored.tmp").path(), b"temporary").unwrap();
    fs::write(left.join("new/keep").path(), b"include").unwrap();
    fs::write(right.join("destination.TMP").path(), b"preserve").unwrap();
    fs::write(right.join("root-only").path(), b"preserve").unwrap();
    fs::write(left.join("new/root-only").path(), b"include nested").unwrap();
    let exclusions = Exclusions::parse("cache\n*.tmp\n./root-only").unwrap();
    let mut progress = Progress::default();
    let plan = SyncPlan::build(
        [left.clone(), right.clone()],
        true,
        exclusions,
        &mut progress,
    )
    .unwrap();
    assert_eq!(
        rows(&plan),
        [
            ("new", SyncChange::Copy),
            ("new/keep", SyncChange::Copy),
            ("new/root-only", SyncChange::Copy)
        ]
        .map(|(path, change)| (PathBuf::from(path), change))
        .into()
    );
    assert_eq!(plan.excluded_count(), 4);
    plan.apply(&mut progress).result.unwrap();
    assert_eq!(fs::read(right.join("new/keep").path()).unwrap(), b"include");
    assert_eq!(
        fs::read(right.join("new/root-only").path()).unwrap(),
        b"include nested"
    );
    assert!(!right.join("new/cache").path().exists());
    assert!(!right.join("new/ignored.tmp").path().exists());
    assert_eq!(
        fs::read(right.join("destination.TMP").path()).unwrap(),
        b"preserve"
    );
    assert_eq!(
        fs::read(right.join("root-only").path()).unwrap(),
        b"preserve"
    );
}

#[test]
fn excluded_child_blocks_a_parent_replacement_or_deletion() {
    let (_temporary, pair) = folders();
    fs::create_dir_all(pair[1].join("branch/deep").path()).unwrap();
    let protected = pair[1].join("branch/deep/keep.dat");
    fs::write(protected.path(), b"protect me").unwrap();
    for replacement in [false, true] {
        if replacement {
            fs::write(pair[0].join("branch").path(), b"regular file").unwrap();
        }
        let error = SyncPlan::build(
            pair.clone(),
            true,
            Exclusions::parse("branch/deep/keep.???").unwrap(),
            &mut Progress::default(),
        )
        .err()
        .expect("the plan would remove an excluded descendant");
        assert!(error.to_string().contains("branch/deep/keep.dat"));
        assert_eq!(fs::read(protected.path()).unwrap(), b"protect me");
    }
}

pub(in crate::vfs) fn sync_round_trip(remote: &Location) {
    use super::super::{sftp, transfer::remote_source_matches};

    let local = tempfile::tempdir().unwrap();
    let remote_fixture = tempfile::tempdir_in(remote.path()).unwrap();
    let source: Location = local.path().to_owned().into();
    let destination = remote.with_path(remote_fixture.path().to_owned());
    let peer = destination.join("changed.bin");
    fs::write(source.join("changed.bin").path(), b"want").unwrap();
    fs::write(source.join(".new").path(), b"copy").unwrap();
    fs::write(peer.path(), b"have").unwrap();
    fs::write(destination.join("extra").path(), b"extra").unwrap();
    let time = UNIX_EPOCH + Duration::from_secs(1_650_000_000);
    set_modified(peer.path(), time);
    let mut progress = Progress::default();
    let plan = SyncPlan::build(
        [source.clone(), destination.clone()],
        true,
        Exclusions::default(),
        &mut progress,
    )
    .unwrap();
    assert_eq!((plan.copies, plan.replacements, plan.deletions), (1, 1, 1));
    plan.verify(&mut progress).unwrap();
    let channel = sftp::remote(&peer).unwrap();
    let original = channel.lstat(peer.path()).unwrap();
    fs::write(peer.path(), b"edit").unwrap();
    set_modified(peer.path(), time);
    let edited = channel.lstat(peer.path()).unwrap();
    assert!(remote_source_matches(&original, &edited));
    let error = plan.verify(&mut progress).unwrap_err();
    assert!(error.to_string().contains("changed.bin"));
    assert!(plan.apply(&mut progress).result.is_err());
    assert_eq!(fs::read(peer.path()).unwrap(), b"edit");
    assert!(destination.join("extra").path().is_file());
    assert!(!destination.join(".new").path().exists());

    let reverse = SyncPlan::build(
        [destination.clone(), source.clone()],
        false,
        Exclusions::default(),
        &mut progress,
    )
    .unwrap();
    assert_eq!(
        (reverse.copies, reverse.replacements, reverse.deletions),
        (1, 1, 0)
    );
    reverse.apply(&mut progress).result.unwrap();
    assert_eq!(
        fs::read(source.join("changed.bin").path()).unwrap(),
        b"edit"
    );
    assert_eq!(fs::read(source.join("extra").path()).unwrap(), b"extra");
    assert_eq!(fs::read(peer.path()).unwrap(), b"edit");
    assert_eq!(fs::read(source.join(".new").path()).unwrap(), b"copy");

    fs::write(source.join("changed.bin").path(), b"new!").unwrap();
    fs::remove_file(source.join("extra").path()).unwrap();
    fs::create_dir_all(destination.join("extra-tree/old").path()).unwrap();
    fs::write(destination.join("extra-tree/old/file").path(), b"old").unwrap();
    symlink(".new", source.join("link").path()).unwrap();
    let upload = SyncPlan::build(
        [source.clone(), destination.clone()],
        true,
        Exclusions::default(),
        &mut progress,
    )
    .unwrap();
    assert_eq!(
        (upload.copies, upload.replacements, upload.deletions),
        (2, 1, 4)
    );
    upload.apply(&mut progress).result.unwrap();
    assert_eq!(fs::read(peer.path()).unwrap(), b"new!");
    assert_eq!(fs::read(destination.join(".new").path()).unwrap(), b"copy");
    assert_eq!(
        fs::read_link(destination.join("link").path()).unwrap(),
        Path::new(".new")
    );
    assert!(!destination.join("extra").path().exists());
    assert!(!destination.join("extra-tree").path().exists());
    assert_eq!(
        fs::read(source.join("changed.bin").path()).unwrap(),
        b"new!"
    );
    assert!(fs::read_dir(destination.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pawcommander-")
    }));

    fs::create_dir_all(source.join("new-folder/private").path()).unwrap();
    fs::write(source.join("new-folder/private/keep").path(), b"local only").unwrap();
    fs::write(source.join("new-folder/include").path(), b"copy").unwrap();
    fs::write(destination.join("remote.log").path(), b"remote only").unwrap();
    let plan = SyncPlan::build(
        [source.clone(), destination.clone()],
        true,
        Exclusions::parse("new-folder/private\n*.log").unwrap(),
        &mut progress,
    )
    .unwrap();
    assert_eq!(plan.excluded_count(), 2);
    plan.apply(&mut progress).result.unwrap();
    assert_eq!(
        fs::read(destination.join("new-folder/include").path()).unwrap(),
        b"copy"
    );
    assert_eq!(
        fs::read(destination.join("remote.log").path()).unwrap(),
        b"remote only"
    );
    assert!(!destination.join("new-folder/private").path().exists());
}
