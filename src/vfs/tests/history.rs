use super::*;

#[test]
fn copy_hash_failure_keeps_partial_batch_and_nested_history_truthful() {
    use crate::result_history::Outcome;
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let published = root.join("published");
    let source = create_directory(&root, "tree").unwrap();
    let pending = root.join("pending");
    let destination = create_directory(&root, "destination").unwrap();
    fs::write(published.path(), b"already copied").unwrap();
    fs::write(pending.path(), b"not started").unwrap();
    for name in ["a", "b", "c"] {
        fs::write(source.join(name).path(), name).unwrap();
    }
    let inspect = destination.clone();
    let mut corrupted = false;
    let cancel = Cancellation::at_checkpoint(move || {
        if !corrupted {
            for entry in fs::read_dir(inspect.path()).unwrap().map(Result::unwrap) {
                let path = entry.path().join("payload/b");
                if path.is_file() && path.metadata().unwrap().len() == 1 {
                    fs::write(path, b"x").unwrap();
                    corrupted = true;
                    break;
                }
            }
        }
        false
    });
    let mut progress = Progress::with_cancellation(cancel);
    progress.set_copy_verification(true);
    let result = transfer_with_conflicts(
        &[published.clone(), source.clone(), pending.clone()],
        &destination,
        TransferKind::Copy,
        &mut progress,
        |_| panic!("no conflict"),
        |_, _| panic!("a copy must not consume cut sources"),
    );
    assert!(result.as_ref().unwrap_err().contains("SHA-256"));
    let record = std::mem::take(&mut progress.results)
        .finish("copy".into(), 0, &result.map(|_| "Copied".into()))
        .unwrap();
    let row = |source: &Location| {
        record
            .items
            .iter()
            .find(|row| row.source == source.display())
            .unwrap()
    };
    assert_eq!(row(&published).outcome, Outcome::Completed);
    assert_eq!(row(&source).outcome, Outcome::Failed);
    assert_eq!(
        row(&source.join("a")).outcome,
        Outcome::PublicationUnconfirmed
    );
    assert_eq!(row(&source.join("b")).outcome, Outcome::Failed);
    assert!(row(&source.join("b"))
        .reason
        .as_ref()
        .unwrap()
        .contains("SHA-256"));
    assert_eq!(row(&source.join("c")).outcome, Outcome::NotStarted);
    assert_eq!(row(&pending).outcome, Outcome::NotStarted);
    assert_eq!(
        fs::read(destination.join("published").path()).unwrap(),
        b"already copied"
    );
    assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 1);
    for name in ["a", "b", "c"] {
        assert_eq!(fs::read(source.join(name).path()).unwrap(), name.as_bytes());
    }
    assert_eq!(fs::read(pending.path()).unwrap(), b"not started");
}

#[test]
fn nested_result_history_publishes_entire_manifest_at_actual_keep_both_target() {
    use crate::result_history::Outcome;
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let source = create_directory(&root, "tree").unwrap();
    create_directory(&source, "nested").unwrap();
    create_directory(&source, "empty").unwrap();
    fs::write(source.join("a").path(), b"first").unwrap();
    fs::write(source.join("nested/b").path(), b"second").unwrap();
    let destination = create_directory(&root, "destination").unwrap();
    create_directory(&destination, "tree").unwrap();
    fs::write(destination.join("tree/old").path(), b"old folder").unwrap();
    let mut progress = Progress::default();
    let summary = transfer_with_conflicts(
        std::slice::from_ref(&source),
        &destination,
        TransferKind::Copy,
        &mut progress,
        |_| {
            Ok(Answer {
                decision: Decision::KeepBoth,
                apply_to_group: false,
            })
        },
        |_, _| panic!("copy"),
    )
    .unwrap();
    assert_eq!(summary.completed, 1);
    assert_eq!(progress.results.items.len(), 5);
    assert!(progress.results.items[0].descendants_enumerated);
    let expected_sources = [
        source.clone(),
        source.join("a"),
        source.join("empty"),
        source.join("nested"),
        source.join("nested/b"),
    ];
    for row in &progress.results.items {
        assert_eq!(row.outcome, Outcome::Completed);
        let original = expected_sources
            .iter()
            .find(|path| path.display() == row.source)
            .unwrap();
        let relative = original.path().strip_prefix(source.path()).unwrap();
        let final_root = destination.join("tree (2)");
        let expected_target = if relative.as_os_str().is_empty() {
            final_root
        } else {
            final_root.join(relative)
        };
        assert_eq!(row.target, Some(expected_target.display()));
        if row.parent.is_some() {
            assert_eq!(row.parent, Some(0));
        }
    }
    assert_eq!(
        fs::read(destination.join("tree (2)/nested/b").path()).unwrap(),
        b"second"
    );
    assert_eq!(
        fs::read(destination.join("tree/old").path()).unwrap(),
        b"old folder"
    );
    assert_eq!(fs::read(source.join("nested/b").path()).unwrap(), b"second");
    let record = std::mem::take(&mut progress.results)
        .finish("copy".into(), 0, &Ok("Copied".into()))
        .unwrap();
    let encoded = serde_json::to_vec(&record).unwrap();
    let reopened: crate::result_history::Record = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(reopened.items.len(), 5);
    assert!(reopened
        .items
        .iter()
        .all(|row| row.outcome == Outcome::Completed));
}

#[test]
fn result_history_keeps_atomic_directory_moves_explicitly_grouped() {
    use crate::result_history::Outcome;
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let source = create_directory(&root, "tree").unwrap();
    create_directory(&source, "nested").unwrap();
    fs::write(source.join("nested/a").path(), b"original").unwrap();
    let destination = create_directory(&root, "destination").unwrap();
    let mut progress = Progress::default();
    let summary = transfer_with_conflicts(
        std::slice::from_ref(&source),
        &destination,
        TransferKind::Move,
        &mut progress,
        |_| panic!("no conflict"),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(summary.completed, 1);
    assert_eq!(progress.results.items.len(), 1);
    assert_eq!(progress.results.items[0].outcome, Outcome::Completed);
    assert!(!progress.results.items[0].descendants_enumerated);
    assert!(progress.results.items[0].parent.is_none());
    assert!(!source.path().exists());
    assert_eq!(
        fs::read(destination.join("tree/nested/a").path()).unwrap(),
        b"original"
    );
}

#[test]
#[cfg(unix)]
fn result_history_failed_manifest_never_claims_descendant_coverage() {
    use crate::result_history::Outcome;
    use std::os::unix::ffi::OsStrExt;
    for generic in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let root: Location = fixture.path().to_path_buf().into();
        let source = create_directory(&root, "tree").unwrap();
        let checked = source.join("a-checked");
        let failed = source.join("b-fifo");
        let unvisited = source.join("c-unvisited");
        fs::write(checked.path(), b"checked but never copied").unwrap();
        fs::write(unvisited.path(), b"untouched").unwrap();
        let name = std::ffi::CString::new(failed.path().as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let target = root.join("target");
        let mut progress = Progress::default();
        progress
            .results
            .plan([(source.clone(), Some(target.display()))]);
        progress.results.begin(&source);
        let result = if generic {
            copy_transfer_cancellable(
                &TransferItem::new(source.clone(), source.clone(), target.clone()),
                None,
                &mut progress,
                &mut |_, _| Ok(()),
                || panic!("unsupported source must not publish"),
            )
        } else {
            crate::fs_ops::copy_atomically_cancellable(
                source.path(),
                target.path(),
                &mut progress,
                |_, _| panic!("unsupported source must not publish"),
                &mut |_| Ok(()),
            )
        };
        assert!(result.is_err());
        let record = std::mem::take(&mut progress.results)
            .finish(
                "copy".into(),
                0,
                &result.map(|_| "Copied".into()).map_err(|e| e.to_string()),
            )
            .unwrap();
        assert_eq!(record.items.len(), 3);
        assert!(!record.items[0].descendants_enumerated);
        assert_eq!(record.items[0].outcome, Outcome::Failed);
        let row = |path: &Location| {
            record
                .items
                .iter()
                .find(|r| r.source == path.display())
                .unwrap()
        };
        assert_eq!(row(&checked).outcome, Outcome::NotStarted);
        assert_eq!(row(&failed).outcome, Outcome::Failed);
        assert!(row(&failed).reason.as_ref().is_some_and(|s| !s.is_empty()));
        assert_eq!(row(&failed).target, Some(target.join("b-fifo").display()));
        assert_eq!(row(&failed).parent, Some(0));
        assert!(!record.items.iter().any(|r| r.source == unvisited.display()));
        let reopened: crate::result_history::Record =
            serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
        assert_eq!(reopened.items.len(), 3);
        assert!(!reopened.items[0].descendants_enumerated);
        assert_eq!(reopened.items[2].outcome, Outcome::Failed);
        assert!(!target.path().exists());
        assert_eq!(
            fs::read(checked.path()).unwrap(),
            b"checked but never copied"
        );
        assert_eq!(fs::read(unvisited.path()).unwrap(), b"untouched");
        assert_eq!(
            fs::read_dir(root.path()).unwrap().count(),
            1,
            "no staging is created"
        );
    }
}

#[test]
fn result_history_cancelled_manifest_retains_only_observed_unpublished_entries() {
    use crate::result_history::Outcome;
    for generic in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let root: Location = fixture.path().to_path_buf().into();
        let source = create_directory(&root, "tree").unwrap();
        for index in 0..32 {
            fs::write(source.join(format!("file-{index:02}")).path(), b"untouched").unwrap();
        }
        let target = root.join("target");
        let mut checks = 0;
        let cancel = Cancellation::at_checkpoint(move || {
            checks += 1;
            checks == if generic { 100 } else { 55 }
        });
        let mut progress = Progress::with_cancellation(cancel);
        progress
            .results
            .plan([(source.clone(), Some(target.display()))]);
        progress.results.begin(&source);
        let result = if generic {
            copy_transfer_cancellable(
                &TransferItem::new(source.clone(), source.clone(), target.clone()),
                None,
                &mut progress,
                &mut |_, _| Ok(()),
                || panic!("cancelled manifest must not publish"),
            )
        } else {
            crate::fs_ops::copy_atomically_cancellable(
                source.path(),
                target.path(),
                &mut progress,
                |_, _| panic!("cancelled manifest must not publish"),
                &mut |_| Ok(()),
            )
        };
        assert!(matches!(result, Err(TransferError::Cancelled)));
        progress.results.cancelled = true;
        let record = std::mem::take(&mut progress.results)
            .finish("copy".into(), 0, &Ok("Stopped".into()))
            .unwrap();
        assert!(!record.items[0].descendants_enumerated);
        assert_eq!(record.items[0].outcome, Outcome::Cancelled);
        assert!(record.items.len() > 1 && record.items.len() < 33);
        assert!(record.items[1..]
            .iter()
            .any(|r| r.outcome == Outcome::Cancelled));
        assert!(record
            .items
            .iter()
            .all(|r| matches!(r.outcome, Outcome::NotStarted | Outcome::Cancelled)));
        assert!(!record
            .items
            .iter()
            .any(|r| r.source == source.join("file-31").display()));
        assert!(!target.path().exists());
        for index in 0..32 {
            assert_eq!(
                fs::read(source.join(format!("file-{index:02}")).path()).unwrap(),
                b"untouched"
            );
        }
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }
}

#[test]
fn nested_result_history_keeps_private_copies_unconfirmed_after_later_failure() {
    use crate::result_history::Outcome;
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let source = create_directory(&root, "tree").unwrap();
    let destination = create_directory(&root, "destination").unwrap();
    for name in ["a", "b", "c"] {
        fs::write(source.join(name).path(), name).unwrap();
    }
    let inspect_destination = destination.clone();
    let change = source.join("b");
    let mut changed = false;
    let cancellation = Cancellation::at_checkpoint(move || {
        if !changed
            && fs::read_dir(inspect_destination.path())
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.path().join("payload/a").is_file())
        {
            fs::write(change.path(), b"changed after capture").unwrap();
            changed = true;
        }
        false
    });
    let mut progress = Progress::with_cancellation(cancellation);
    let result = transfer_with_conflicts(
        std::slice::from_ref(&source),
        &destination,
        TransferKind::Copy,
        &mut progress,
        |_| panic!("no conflict"),
        |_, _| panic!("copy"),
    );
    assert!(result.is_err());
    let record = std::mem::take(&mut progress.results)
        .finish("copy".into(), 0, &result.map(|_| "Copied".into()))
        .unwrap();
    let row = |name: &str| {
        record
            .items
            .iter()
            .find(|row| row.source == source.join(name).display())
            .unwrap()
    };
    assert_eq!(record.items[0].outcome, Outcome::Failed);
    assert_eq!(row("a").outcome, Outcome::PublicationUnconfirmed);
    assert_eq!(row("b").outcome, Outcome::Failed);
    assert!(row("b").reason.is_some());
    assert_eq!(row("c").outcome, Outcome::NotStarted);
    assert!(!record.items.iter().any(|row| matches!(
        row.outcome,
        Outcome::Completed | Outcome::CompletedWithWarning
    )));
    assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 0);
    assert_eq!(fs::read(source.join("a").path()).unwrap(), b"a");
    assert_eq!(fs::read(source.join("c").path()).unwrap(), b"c");
}

#[test]
fn nested_result_history_preserves_child_cancellation_when_cleanup_fails() {
    use crate::result_history::Outcome;
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let source = create_directory(&root, "tree").unwrap();
    for name in ["a", "b", "c"] {
        fs::write(source.join(name).path(), name).unwrap();
    }
    let target = create_directory(&root, "target").unwrap();
    fs::write(target.join("precious").path(), b"old destination").unwrap();
    let mut progress = Progress::default();
    progress
        .results
        .plan([(source.clone(), Some(target.display()))]);
    progress.results.begin(&source);
    let cancellation = progress.cancellation();
    let mut obstacle = None;
    let result = copy_transfer_cancellable(
        &TransferItem::new(source.clone(), source.clone(), target.clone()),
        None,
        &mut progress,
        &mut |entry, phase| {
            if *entry == source.join("a") && phase == CopyPhase::EntryCopied {
                let staging = fs::read_dir(root.path())
                    .unwrap()
                    .filter_map(Result::ok)
                    .find(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".pawcommander-transfer-")
                    })
                    .unwrap()
                    .path();
                fs::remove_dir_all(&staging).unwrap();
                fs::write(&staging, b"cleanup obstacle").unwrap();
                obstacle = Some(staging);
                cancellation.request();
            }
            Ok(())
        },
        || panic!("cancelled tree must not publish"),
    );
    assert!(
        matches!(&result, Err(TransferError::Failed(_))),
        "cleanup failure remains a root failure"
    );
    let record = std::mem::take(&mut progress.results)
        .finish(
            "copy".into(),
            0,
            &result
                .map(|_| "Copied".into())
                .map_err(|error| error.to_string()),
        )
        .unwrap();
    let child = |name: &str| {
        record
            .items
            .iter()
            .find(|row| row.source == source.join(name).display())
            .unwrap()
    };
    assert_eq!(record.items[0].outcome, Outcome::Failed);
    assert_eq!(child("a").outcome, Outcome::PublicationUnconfirmed);
    assert_eq!(child("b").outcome, Outcome::Cancelled);
    assert_eq!(child("c").outcome, Outcome::NotStarted);
    assert!(child("b").reason.is_some());
    assert_eq!(
        fs::read(target.join("precious").path()).unwrap(),
        b"old destination"
    );
    for name in ["a", "b", "c"] {
        assert_eq!(fs::read(source.join(name).path()).unwrap(), name.as_bytes());
    }
    assert_eq!(fs::read(obstacle.unwrap()).unwrap(), b"cleanup obstacle");
}

#[test]
fn nested_result_history_generic_transfer_checks_final_source_verification() {
    use crate::result_history::Outcome;
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let source = create_directory(&root, "tree").unwrap();
    fs::write(source.join("a").path(), b"first").unwrap();
    fs::write(source.join("b").path(), b"second").unwrap();
    let target = root.join("target");
    let mut progress = Progress::default();
    progress
        .results
        .plan(vec![(source.clone(), Some(target.display()))]);
    progress.results.begin(&source);
    let result = copy_transfer_cancellable(
        &TransferItem::new(source.clone(), source.clone(), target.clone()),
        None,
        &mut progress,
        &mut |entry, phase| {
            if *entry == source.join("b") && phase == CopyPhase::EntryCopied {
                fs::write(source.join("a").path(), b"changed").unwrap();
            }
            Ok(())
        },
        || panic!("changed tree must not publish"),
    );
    assert!(result.is_err());
    let record = std::mem::take(&mut progress.results)
        .finish(
            "copy".into(),
            0,
            &result
                .map(|_| "Copied".into())
                .map_err(|error| error.to_string()),
        )
        .unwrap();
    assert_eq!(
        record
            .items
            .iter()
            .find(|row| row.source == source.join("a").display())
            .unwrap()
            .outcome,
        Outcome::Failed
    );
    assert_eq!(
        record
            .items
            .iter()
            .find(|row| row.source == source.join("b").display())
            .unwrap()
            .outcome,
        Outcome::PublicationUnconfirmed
    );
    assert!(!target.path().exists());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn result_history_retains_partial_failure_and_actual_keep_both_target() {
    use crate::result_history::Outcome;
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let destination = create_directory(&root, "destination").unwrap();
    let sources: Vec<_> = ["a", "b", "c"].map(|name| root.join(name)).into();
    for source in &sources {
        fs::write(source.path(), source.name()).unwrap();
    }
    fs::write(destination.join("a").path(), b"old destination").unwrap();
    let mut progress = Progress::default();
    let result = transfer_with_conflicts(
        &sources,
        &destination,
        TransferKind::Copy,
        &mut progress,
        |_| {
            fs::remove_file(sources[1].path()).unwrap();
            Ok(Answer {
                decision: Decision::KeepBoth,
                apply_to_group: false,
            })
        },
        |_, _| panic!("copy"),
    );
    assert!(result.is_err());
    let record = std::mem::take(&mut progress.results)
        .finish("copy".into(), 0, &result.map(|_| String::new()))
        .unwrap();
    assert_eq!(
        record
            .items
            .iter()
            .map(|row| row.outcome)
            .collect::<Vec<_>>(),
        [Outcome::Completed, Outcome::Failed, Outcome::NotStarted]
    );
    assert_eq!(
        record.items[0].target,
        Some(destination.join("a (2)").display())
    );
    assert!(record.items[1].reason.as_ref().unwrap().contains('b'));
    assert!(record.items[2].reason.is_some());
    assert_eq!(
        fs::read(destination.join("a").path()).unwrap(),
        b"old destination"
    );
    assert_eq!(fs::read(destination.join("a (2)").path()).unwrap(), b"a");
    assert_eq!(fs::read(sources[0].path()).unwrap(), b"a");
    assert_eq!(fs::read(sources[2].path()).unwrap(), b"c");
    assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 2);
}

#[test]
#[cfg(target_os = "linux")]
fn result_history_distinguishes_non_utf8_paths_with_identical_display() {
    use crate::result_history::Outcome;
    use std::os::unix::ffi::OsStringExt;
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let destination = create_directory(&root, "destination").unwrap();
    let names = [
        OsString::from_vec(b"same\xff".to_vec()),
        OsString::from_vec(b"same\xfe".to_vec()),
    ];
    let sources = names.each_ref().map(|name| root.join(name));
    assert_eq!(sources[0].display(), sources[1].display());
    fs::write(sources[0].path(), b"first source").unwrap();
    fs::write(sources[1].path(), b"second source").unwrap();
    fs::write(destination.join(&names[0]).path(), b"old target").unwrap();
    let mut progress = Progress::default();
    let summary = transfer_with_conflicts(
        &sources,
        &destination,
        TransferKind::Copy,
        &mut progress,
        |_| {
            Ok(Answer {
                decision: Decision::Skip,
                apply_to_group: false,
            })
        },
        |_, _| panic!("copy"),
    )
    .unwrap();
    assert_eq!((summary.completed, summary.skipped), (1, 1));
    assert_eq!(
        progress
            .results
            .items
            .iter()
            .map(|row| row.outcome)
            .collect::<Vec<_>>(),
        [Outcome::Skipped, Outcome::Completed]
    );
    assert_eq!(
        fs::read(destination.join(&names[0]).path()).unwrap(),
        b"old target"
    );
    assert_eq!(
        fs::read(destination.join(&names[1]).path()).unwrap(),
        b"second source"
    );
}
