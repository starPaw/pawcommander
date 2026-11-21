use super::*;
use crate::vfs::{RetryTransfer, TransferFailure};

fn retry_progress(retry: RetryTransfer) -> Progress {
    let mut progress = Progress::default();
    progress.retry = Some(retry);
    progress
}

fn run(
    progress: &mut Progress,
    sources: &[Location],
    target: &Location,
    kind: TransferKind,
    decision: Decision,
) -> Result<TransferSummary, TransferFailure> {
    transfer_with_conflicts(
        sources,
        target,
        kind,
        progress,
        |_| {
            Ok(Answer {
                decision,
                apply_to_group: false,
            })
        },
        |_, _| {},
    )
}

#[test]
fn retry_cancelled_batch_never_repeats_a_published_root() {
    for kind in [TransferKind::Copy, TransferKind::Move] {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        fs::write(&a, "first").unwrap();
        fs::write(&b, "second").unwrap();
        let first_target = target.join("a");
        let cancel = Cancellation::at_checkpoint(move || first_target.exists());
        let target: Location = target.into();
        let mut progress = Progress::with_cancellation(cancel);
        progress.retry = Some(RetryTransfer::new(kind, target.clone()));
        let summary = run(
            &mut progress,
            &[a.clone().into(), b.clone().into()],
            &target,
            kind,
            Decision::KeepBoth,
        )
        .unwrap();
        assert!(summary.cancelled);
        assert_eq!(summary.completed, 1);
        let retry = progress
            .retry
            .take()
            .unwrap()
            .unfinished(&progress.results)
            .unwrap();
        assert_eq!(retry.sources(), vec![b.clone().into()]);
        // A recreated/edited completed source must not become new retry work.
        fs::write(&a, "new independent content").unwrap();
        let sources = retry.sources();
        let mut progress = retry_progress(retry);
        assert_eq!(
            run(&mut progress, &sources, &target, kind, Decision::KeepBoth)
                .unwrap()
                .completed,
            1
        );
        assert!(progress
            .retry
            .take()
            .unwrap()
            .unfinished(&progress.results)
            .is_none());
        assert_eq!(fs::read(target.join("a").path()).unwrap(), b"first");
        assert_eq!(fs::read(&a).unwrap(), b"new independent content");
        assert_eq!(fs::read(target.join("b").path()).unwrap(), b"second");
        assert!(!target.join("a (2)").path().exists());
        assert_eq!(b.exists(), kind == TransferKind::Copy);
    }
}

fn skipped_file() -> (tempfile::TempDir, Location, Location, RetryTransfer) {
    let root = tempfile::tempdir().unwrap();
    let source: Location = root.path().join("source").into();
    let target: Location = root.path().join("target").into();
    fs::create_dir(target.path()).unwrap();
    fs::write(source.path(), "original").unwrap();
    fs::write(target.join("source").path(), "old target").unwrap();
    let mut progress = retry_progress(RetryTransfer::new(TransferKind::Copy, target.clone()));
    assert_eq!(
        run(
            &mut progress,
            std::slice::from_ref(&source),
            &target,
            TransferKind::Copy,
            Decision::Skip
        )
        .unwrap()
        .skipped,
        1
    );
    let retry = progress
        .retry
        .take()
        .unwrap()
        .unfinished(&progress.results)
        .unwrap();
    (root, source, target, retry)
}

#[test]
fn retry_rejects_replaced_source_and_does_not_adopt_new_evidence() {
    let (root, source, target, retry) = skipped_file();
    fs::rename(source.path(), root.path().join("saved-original")).unwrap();
    fs::write(source.path(), "original").unwrap();
    let mut progress = retry_progress(retry);
    for _ in 0..2 {
        assert!(run(
            &mut progress,
            std::slice::from_ref(&source),
            &target,
            TransferKind::Copy,
            Decision::KeepBoth
        )
        .is_err());
        assert_eq!(
            fs::read(target.join("source").path()).unwrap(),
            b"old target"
        );
        assert!(!target.join("source (2)").path().exists());
        progress.retry = progress.retry.take().unwrap().unfinished(&progress.results);
        assert!(progress.retry.is_some());
    }
}

#[test]
fn retry_rechecks_source_after_waiting_for_a_conflict_answer() {
    let (_root, source, target, retry) = skipped_file();
    let mut progress = retry_progress(retry);
    let result = transfer_with_conflicts(
        std::slice::from_ref(&source),
        &target,
        TransferKind::Copy,
        &mut progress,
        |_| {
            fs::write(source.path(), "changed while waiting").unwrap();
            Ok(Answer {
                decision: Decision::KeepBoth,
                apply_to_group: false,
            })
        },
        |_, _| {},
    );
    assert!(result.is_err());
    assert_eq!(
        fs::read(target.join("source").path()).unwrap(),
        b"old target"
    );
    assert_eq!(fs::read(source.path()).unwrap(), b"changed while waiting");
    assert!(!target.join("source (2)").path().exists());
}

#[test]
fn retry_rejects_a_replaced_destination_directory() {
    let (root, source, target, retry) = skipped_file();
    fs::rename(target.path(), root.path().join("old-directory")).unwrap();
    fs::create_dir(target.path()).unwrap();
    let mut progress = retry_progress(retry);
    assert!(run(
        &mut progress,
        std::slice::from_ref(&source),
        &target,
        TransferKind::Copy,
        Decision::KeepBoth
    )
    .is_err());
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
    assert_eq!(
        fs::read(root.path().join("old-directory/source")).unwrap(),
        b"old target"
    );
    assert_eq!(fs::read(source.path()).unwrap(), b"original");
}

#[test]
fn retry_unconfirmed_and_completed_with_warning_are_never_candidates() {
    for outcome in [
        crate::result_history::Outcome::PublicationUnconfirmed,
        crate::result_history::Outcome::CompletedWithWarning,
    ] {
        let (_root, source, target, retry) = skipped_file();
        let mut results = crate::result_history::Results::default();
        results.plan([(source.clone(), Some(target.join("source").display()))]);
        results.record(
            &source,
            None,
            outcome,
            Some("lost publication reply or cleanup failure".into()),
        );
        assert!(retry.unfinished(&results).is_none());
        assert_eq!(
            fs::read(target.join("source").path()).unwrap(),
            b"old target"
        );
    }
}

#[test]
fn retry_cancelled_directory_rebuilds_private_copy_and_preserves_empty_folders() {
    let root = tempfile::tempdir().unwrap();
    let source: Location = root.path().join("tree").into();
    let target: Location = root.path().join("target").into();
    fs::create_dir_all(source.join("empty").path()).unwrap();
    fs::create_dir(target.path()).unwrap();
    fs::write(source.join("file").path(), "payload").unwrap();
    let observed = target.clone();
    let cancel = Cancellation::at_checkpoint(move || {
        fs::read_dir(observed.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".pawcommander-copy-")
            })
    });
    let mut progress = Progress::with_cancellation(cancel);
    progress.retry = Some(RetryTransfer::new(TransferKind::Copy, target.clone()));
    assert!(
        run(
            &mut progress,
            std::slice::from_ref(&source),
            &target,
            TransferKind::Copy,
            Decision::KeepBoth
        )
        .unwrap()
        .cancelled
    );
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
    let retry = progress
        .retry
        .take()
        .unwrap()
        .unfinished(&progress.results)
        .unwrap();
    let mut progress = retry_progress(retry);
    assert_eq!(
        run(
            &mut progress,
            std::slice::from_ref(&source),
            &target,
            TransferKind::Copy,
            Decision::KeepBoth
        )
        .unwrap()
        .completed,
        1
    );
    assert_eq!(
        fs::read(target.join("tree/file").path()).unwrap(),
        b"payload"
    );
    assert!(target.join("tree/empty").path().is_dir());
    assert_eq!(fs::read(source.join("file").path()).unwrap(), b"payload");
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1);
}
