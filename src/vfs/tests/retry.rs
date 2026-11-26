use super::*;
use crate::file_error::{Failure, RetryProblem};
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

fn assert_retry_stopped(
    result: Result<TransferSummary, TransferFailure>,
    problem: RetryProblem,
    location: &Location,
) {
    let failure = result.expect_err("retry must stop");
    assert_eq!(failure.summary.completed, 0);
    assert!(
        matches!(
            failure.error,
            TransferError::Failed(Failure::Retry(ref reason))
                if reason.problem == problem && &reason.location == location
        ),
        "{:?}",
        failure.error
    );
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
        let result = run(
            &mut progress,
            std::slice::from_ref(&source),
            &target,
            TransferKind::Copy,
            Decision::KeepBoth,
        );
        assert_retry_stopped(result, RetryProblem::SourceChanged, &source);
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
    assert_retry_stopped(result, RetryProblem::SourceChanged, &source);
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
    let result = run(
        &mut progress,
        std::slice::from_ref(&source),
        &target,
        TransferKind::Copy,
        Decision::KeepBoth,
    );
    assert_retry_stopped(result, RetryProblem::DestinationChanged, &target);
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
    assert_eq!(
        fs::read(root.path().join("old-directory/source")).unwrap(),
        b"old target"
    );
    assert_eq!(fs::read(source.path()).unwrap(), b"original");
}

#[test]
fn retry_uses_the_original_operation_and_destination() {
    let (root, source, target, retry) = skipped_file();
    let other: Location = root.path().join("other-target").into();
    fs::create_dir(other.path()).unwrap();
    for (destination, kind, problem) in [
        (&other, TransferKind::Copy, RetryProblem::DestinationChanged),
        (&target, TransferKind::Move, RetryProblem::RequestChanged),
    ] {
        let mut progress = retry_progress(retry.clone());
        let result = run(
            &mut progress,
            std::slice::from_ref(&source),
            destination,
            kind,
            Decision::KeepBoth,
        );
        assert_retry_stopped(result, problem, destination);
    }
    assert_eq!(fs::read_dir(other.path()).unwrap().count(), 0);
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1);
    assert_eq!(
        fs::read(target.join("source").path()).unwrap(),
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
fn retry_excludes_both_sides_of_an_overlap_but_keeps_similar_names() {
    let dir = tempfile::tempdir().unwrap();
    let root: Location = dir.path().to_owned().into();
    let destination = root.join("destination");
    fs::create_dir(destination.path()).unwrap();
    let tree = root.join("tree");
    let child = tree.join("child");
    let sibling = root.join("tree-old");
    fs::create_dir(tree.path()).unwrap();
    fs::write(child.path(), b"child").unwrap();
    fs::write(sibling.path(), b"sibling").unwrap();
    let roots = [tree, child, sibling];
    let plan = roots.clone().map(|source| {
        let target = destination.join(source.path().file_name().unwrap());
        TransferItem::new(source.clone(), source, target)
    });
    let mut retry = RetryTransfer::new(TransferKind::Copy, destination);
    retry.prepare(&plan, &Cancellation::default()).unwrap();
    for published in &roots[..2] {
        let mut results = crate::result_history::Results::default();
        results.plan(roots.iter().cloned().map(|root| (root, None)));
        results.record(
            published,
            None,
            crate::result_history::Outcome::Completed,
            None,
        );
        let unfinished = retry.clone().unfinished(&results).unwrap();
        assert_eq!(unfinished.sources(), roots[2..]);
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
