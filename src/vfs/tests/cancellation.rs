use super::*;

#[test]
fn cancellation_reports_normalized_batch_remainder_and_skips() {
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let destination = create_directory(&root, "destination").unwrap();
    let first = root.join("first");
    let skipped = root.join("skipped");
    let tree = create_directory(&root, "tree").unwrap();
    let third = root.join("third");
    fs::write(first.path(), b"first payload").unwrap();
    fs::write(skipped.path(), b"skip source").unwrap();
    fs::write(destination.join("skipped").path(), b"old destination").unwrap();
    fs::write(tree.join("large.bin").path(), vec![0x5a; 3 * 1024 * 1024]).unwrap();
    fs::write(third.path(), b"third payload").unwrap();
    let inspect_destination = destination.clone();
    let cancel = Cancellation::at_checkpoint(move || {
        inspect_destination.join("first").path().exists()
            && fs::read_dir(inspect_destination.path())
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".pawcommander-copy-")
                        && fs::metadata(entry.path().join("payload/large.bin"))
                            .is_ok_and(|meta| meta.len() >= 1024 * 1024)
                })
    });
    let mut progress = Progress::with_cancellation(cancel);
    let report = transfer_with_conflicts(
        &[
            first.clone(),
            skipped.clone(),
            tree.clone(),
            tree.join("large.bin"),
            tree.clone(),
            third.clone(),
        ],
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
    assert!(report.cancelled);
    assert!(!report.preparing);
    assert_eq!((report.completed, report.skipped), (1, 1));
    assert_eq!(report.remaining, vec![tree.clone(), third.clone()]);
    use crate::result_history::Outcome;
    assert_eq!(
        progress
            .results
            .items
            .iter()
            .filter(|row| row.parent.is_none())
            .map(|row| row.outcome)
            .collect::<Vec<_>>(),
        [
            Outcome::Completed,
            Outcome::Skipped,
            Outcome::Cancelled,
            Outcome::NotStarted
        ]
    );
    let record = std::mem::take(&mut progress.results)
        .finish("copy".into(), 0, &Ok("Stopped".into()))
        .unwrap();
    assert!(record.items[3].reason.is_some());
    let child = record
        .items
        .iter()
        .find(|row| row.source == tree.join("large.bin").display())
        .unwrap();
    assert_eq!(child.outcome, Outcome::Cancelled);
    assert_eq!(
        child.target,
        Some(destination.join("tree/large.bin").display())
    );
    assert_eq!(
        fs::read(destination.join("first").path()).unwrap(),
        b"first payload"
    );
    assert_eq!(
        fs::read(destination.join("skipped").path()).unwrap(),
        b"old destination"
    );
    assert!(!destination.join("tree").path().exists());
    assert!(!destination.join("third").path().exists());
    assert_eq!(
        fs::read(tree.join("large.bin").path()).unwrap(),
        vec![0x5a; 3 * 1024 * 1024]
    );
    assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 2);
}

#[test]
fn preparation_and_conflict_cancellation_keep_pending_identities() {
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let destination = create_directory(&root, "destination").unwrap();
    let source = root.join("source");
    fs::write(source.path(), b"new").unwrap();
    fs::write(destination.join("source").path(), b"old").unwrap();
    let mut progress = Progress::default();
    progress.cancellation().request();
    let report = transfer_with_conflicts(
        &[source.clone(), source.clone()],
        &destination,
        TransferKind::Copy,
        &mut progress,
        |_| panic!("preparing"),
        |_, _| panic!("copy"),
    )
    .unwrap();
    assert!(report.preparing && report.cancelled);
    assert_eq!(report.remaining, vec![source.clone(), source.clone()]);
    let mut progress = Progress::default();
    let cancel = progress.cancellation();
    let report = transfer_with_conflicts(
        std::slice::from_ref(&source),
        &destination,
        TransferKind::Copy,
        &mut progress,
        |_| {
            cancel.request();
            Ok(Answer {
                decision: Decision::Replace,
                apply_to_group: false,
            })
        },
        |_, _| panic!("copy"),
    )
    .unwrap();
    assert!(report.cancelled && !report.preparing);
    assert_eq!(report.remaining, vec![source]);
    assert_eq!(fs::read(destination.join("source").path()).unwrap(), b"old");
    assert_eq!(report.completed, 0);
}

#[test]
fn move_cancellation_preserves_committed_events_and_late_final_stop_is_success() {
    for count in [1, 3] {
        let fixture = tempfile::tempdir().unwrap();
        let root: Location = fixture.path().to_path_buf().into();
        let destination = create_directory(&root, "destination").unwrap();
        let sources: Vec<_> = (0..count)
            .map(|index| root.join(format!("file-{index}")))
            .collect();
        for source in &sources {
            fs::write(source.path(), b"payload").unwrap();
        }
        let mut progress = Progress::default();
        let cancel = progress.cancellation();
        let mut moved = Vec::new();
        let report = transfer_with_conflicts(
            &sources,
            &destination,
            TransferKind::Move,
            &mut progress,
            |_| panic!("no conflicts"),
            |source, _| {
                moved.push(source.clone());
                cancel.request();
            },
        )
        .unwrap();
        assert_eq!(moved, vec![sources[0].clone()]);
        assert_eq!(report.completed, 1);
        assert_eq!(report.cancelled, count != 1);
        assert_eq!(report.remaining, sources[1..]);
        assert!(!sources[0].path().exists());
        assert_eq!(
            fs::read(destination.join("file-0").path()).unwrap(),
            b"payload"
        );
        for source in &sources[1..] {
            assert!(source.path().exists());
        }
    }
}

#[test]
fn cancellation_and_committed_cleanup_failures_are_real_errors_with_truthful_accounting() {
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let source = root.join("source");
    let target = root.join("target");
    let pending = root.join("pending");
    fs::write(source.path(), b"payload").unwrap();
    let mut committed = false;
    let mut retained = None;
    let result = fs_ops::copy_atomically_cancellable(
        source.path(),
        target.path(),
        &mut Progress::default(),
        |payload, destination| {
            fs::rename(payload, destination).unwrap();
            committed = true;
            let staging = payload.parent().unwrap();
            fs::remove_dir(staging).unwrap();
            // Deterministic cleanup failure independent of uid/permissions.
            fs::write(staging, b"cleanup obstacle").unwrap();
            retained = Some(staging.to_path_buf());
            Ok(())
        },
        &mut |_| Ok(()),
    );
    let mut summary = TransferSummary {
        remaining: vec![source.clone(), pending.clone()],
        ..TransferSummary::default()
    };
    let error = record_item_result(&mut summary, &source, committed, result).unwrap_err();
    assert!(matches!(error, TransferError::Failed(_)));
    assert_eq!(summary.completed, 1);
    assert_eq!(summary.remaining, vec![pending]);
    let retained = retained.unwrap();
    assert!(error.to_string().contains(&retained.display().to_string()));
    assert_eq!(fs::read(&retained).unwrap(), b"cleanup obstacle");
    assert_eq!(fs::read(target.path()).unwrap(), b"payload");
    let cancelled_error = staging_result_typed(
        Err(TransferError::Cancelled),
        Err("cleanup denied".into()),
        &root,
        &target,
    )
    .unwrap_err();
    assert!(matches!(cancelled_error, TransferError::Failed(_)));
    assert!(cancelled_error.to_string().contains("cleanup denied"));
}

#[test]
fn replacement_critical_section_defers_cancellation_through_publication_and_cleanup() {
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let source = root.join("source");
    let target = root.join("target");
    fs::write(source.path(), b"new payload").unwrap();
    fs::write(target.path(), b"old payload").unwrap();
    let expected = entry_metadata(&target).unwrap().unwrap();
    let cancel = Cancellation::default();
    with_staging(&target, |staging, preserve| {
        publish_replacement_with_backup_rename(
            &source,
            &target,
            staging,
            &expected,
            preserve,
            |from, to| {
                rename_without_replacing(from, to)?;
                cancel.request();
                Ok(())
            },
        )
    })
    .unwrap();
    assert!(cancel.is_requested());
    assert_eq!(fs::read(target.path()).unwrap(), b"new payload");
    assert!(!source.path().exists());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}
