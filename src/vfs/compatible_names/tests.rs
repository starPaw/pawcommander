use super::*;
use crate::{
    file_error::{
        AccessCause, Failure, MutationError, PublicationError, ScanError, TransferPlanError,
    },
    result_history::Outcome,
};
use std::ffi::OsStr;

#[test]
fn repairs_reserved_names_length_and_unicode_without_extending_over_limits() {
    let destination = Location::Local("/destination".into());
    let proposed = Name::suggest(
        OsStr::new("cafe\u{301}:plan?.txt. "),
        255,
        false,
        &destination,
    )
    .unwrap();
    assert_eq!(proposed.value, "café_plan_.txt");
    assert!(proposed.reasons.contains(&Reason::Characters));
    assert!(proposed.reasons.contains(&Reason::Trailing));
    assert!(proposed.reasons.contains(&Reason::Unicode));
    let device = Name::suggest(OsStr::new("COM¹.tar.gz"), 255, false, &destination).unwrap();
    assert_eq!(device.value, "_COM¹.tar.gz");
    assert_eq!(device.reasons, [Reason::Reserved]);
    let native =
        Name::suggest(OsStr::from_bytes(b"raw\xff.txt"), 255, false, &destination).unwrap();
    assert_eq!(native.value, "raw\u{fffd}.txt");
    assert_eq!(native.reasons, [Reason::Encoding]);

    let long = format!("{}.md", "🙂".repeat(128));
    let first = Name::suggest(OsStr::new(&long), 34, false, &destination).unwrap();
    let mut occupied = HashSet::from([portable_key(OsStr::new(&first.value))]);
    let second = first
        .reserve(34, false, &destination, &mut occupied, |_| Ok(false))
        .unwrap();
    assert!(second.value.len() <= 34);
    assert!(second.value.ends_with(" (2).md"));
    assert_eq!(second.reasons, [Reason::Length, Reason::Collision]);
    let utf16 = Name::suggest(OsStr::new(&long), 1024, false, &destination).unwrap();
    assert!(utf16.value.encode_utf16().count() <= 255);
    assert!(utf16.value.ends_with(".md"));
    assert_eq!(
        portable_key(OsStr::new("Café")),
        portable_key(OsStr::new("cafe\u{301}"))
    );
    assert_eq!(
        portable_key(OsStr::new("Straße")),
        portable_key(OsStr::new("STRAẞE"))
    );
}

#[test]
fn fat_names_protect_existing_metadata_paths_and_fit_in_decomposed_form() {
    let destination = Location::Local("/destination".into());
    let mut occupied = HashSet::from([portable_key(OsStr::new("._CAFÉ.TXT"))]);
    let suggestion = Name::suggest(OsStr::new("café.txt"), 24, true, &destination)
        .unwrap()
        .reserve(24, true, &destination, &mut occupied, |_| Ok(false))
        .unwrap();
    assert_eq!(suggestion.value, "café (2).txt");
    assert_eq!(suggestion.reasons, [Reason::Collision]);
    assert!(occupied.contains(&portable_key(OsStr::new("._café (2).txt"))));

    let explicit = Name::suggest(OsStr::new("._notes"), 24, true, &destination).unwrap();
    assert_eq!(explicit.value, "__notes");
    assert_eq!(explicit.reasons, [Reason::MetadataName]);

    let long = Name::suggest(OsStr::new("ééé.txt"), 11, true, &destination).unwrap();
    assert_eq!(long.value, "éé.txt");
    assert_eq!(long.reasons, [Reason::Length]);
}

#[test]
fn name_allocation_distinguishes_exhaustion_from_insufficient_room() {
    let destination = Location::Local("/destination/nested".into());
    let proposed = OsStr::new("data");
    let target = destination.join(proposed);
    let reserve = |bytes| {
        Name::suggest(proposed, bytes, false, &destination)
            .unwrap()
            .reserve(bytes, false, &destination, &mut HashSet::new(), |_| {
                Ok(true)
            })
            .err()
            .expect("no candidate was available")
    };
    assert!(
        matches!(reserve(4), TransferError::Failed(Failure::CompatiblePlan(reason))
        if matches!(reason.as_ref(), CompatiblePlanError::NameLimit { target: path, available: 4 } if path == &target))
    );
    assert!(
        matches!(reserve(255), TransferError::Failed(Failure::TransferPlan(reason))
        if matches!(reason.as_ref(), TransferPlanError::NoUnusedName(path) if path == &target))
    );
    assert!(matches!(
        Name::suggest(proposed, 255, false, &destination)
            .unwrap()
            .reserve(255, false, &destination, &mut HashSet::new(), |_| Err(
                TransferError::Cancelled
            )),
        Err(TransferError::Cancelled)
    ));
}

fn folders() -> (tempfile::TempDir, Location, Location) {
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let source = Location::Local(root.path().join("left"));
    let target = Location::Local(root.path().join("right"));
    fs::create_dir(source.path()).unwrap();
    fs::create_dir(target.path()).unwrap();
    (root, source, target)
}

#[test]
fn preview_failures_keep_the_exact_child_and_the_volume_probe_cause() {
    let (_root, source, target) = folders();
    let folder = source.join("reports");
    let link = folder.join("alias.txt");
    fs::create_dir(folder.path()).unwrap();
    std::os::unix::fs::symlink("missing.txt", link.path()).unwrap();
    let result = build(
        &[folder],
        &target,
        TransferKind::Copy,
        &mut Progress::default(),
    );
    assert!(
        matches!(result, Err(TransferError::Failed(Failure::CompatiblePlan(reason)))
        if matches!(reason.as_ref(), CompatiblePlanError::UnsupportedItem(path) if path == &link))
    );
    assert_eq!(
        fs::read_link(link.path()).unwrap(),
        Path::new("missing.txt")
    );
    assert!(fs::read_dir(target.path()).unwrap().next().is_none());

    let missing = target.join("missing-folder");
    let result = Limits::read(&missing);
    assert!(
        matches!(result, Err(TransferError::Failed(Failure::Access(reason)))
        if reason.location == missing && reason.action == AccessAction::ReadVolumeLimits
        && matches!(&reason.cause, AccessCause::Local(error) if error.raw_os_error() == Some(libc::ENOENT)))
    );
}

#[test]
fn the_depth_limit_identifies_the_first_unscannable_folder() {
    let (_root, source, target) = folders();
    let folder = source.join("reports");
    fs::create_dir(folder.path()).unwrap();
    let mut deepest = folder.clone();
    for _ in 0..=crate::vfs::MAX_TREE_DEPTH {
        deepest = deepest.join("d");
        fs::create_dir(deepest.path()).unwrap();
    }
    let result = build(
        &[folder],
        &target,
        TransferKind::Copy,
        &mut Progress::default(),
    );
    assert!(
        matches!(result, Err(TransferError::Failed(Failure::Scan(reason)))
        if matches!(reason.as_ref(), ScanError::TransferDepthLimit { location, limit }
            if location == &deepest && *limit == crate::vfs::MAX_TREE_DEPTH))
    );
    assert!(fs::read_dir(target.path()).unwrap().next().is_none());
}

#[test]
fn copy_applies_the_reviewed_nested_names_and_preserves_existing_items() {
    let (_root, source, target) = folders();
    let bundle = source.join("Reports");
    fs::create_dir(bundle.path()).unwrap();
    fs::write(bundle.join("a:b.txt").path(), b"changed-name").unwrap();
    fs::write(bundle.join("a_b.txt").path(), b"keep-spelling").unwrap();
    fs::write(target.join("reports").path(), b"occupied by casing").unwrap();
    let plan = build(
        std::slice::from_ref(&bundle),
        &target,
        TransferKind::Copy,
        &mut Progress::default(),
    )
    .unwrap();
    assert_eq!(plan.entries, 3);
    assert_eq!(plan.changes.len(), 2);
    assert_eq!(plan.changes[0].after, Path::new("Reports (2)"));
    assert_eq!(plan.changes[1].after, Path::new("Reports (2)/a_b (2).txt"));
    assert!(!target.join("Reports (2)").path().exists());
    let report = plan.apply(&mut Progress::default()).unwrap();
    assert_eq!(report.completed, 1);
    assert!(report.remaining.is_empty() && !report.cancelled);
    let output = target.join("Reports (2)");
    assert_eq!(
        fs::read(output.join("a_b.txt").path()).unwrap(),
        b"keep-spelling"
    );
    assert_eq!(
        fs::read(output.join("a_b (2).txt").path()).unwrap(),
        b"changed-name"
    );
    assert_eq!(
        fs::read(bundle.join("a:b.txt").path()).unwrap(),
        b"changed-name"
    );
    assert_eq!(
        fs::read(target.join("reports").path()).unwrap(),
        b"occupied by casing"
    );
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 2);
}

#[test]
fn move_verifies_output_before_removing_the_tree_with_its_original_names() {
    let (_root, source, target) = folders();
    let folder = source.join("AUX");
    fs::create_dir_all(folder.join("old:reports").path()).unwrap();
    fs::write(folder.join("old:reports/LPT3.txt").path(), b"verified file").unwrap();
    let plan = build(
        std::slice::from_ref(&folder),
        &target,
        TransferKind::Move,
        &mut Progress::default(),
    )
    .unwrap();
    assert_eq!(plan.changes.len(), 3);
    plan.apply(&mut Progress::default()).unwrap();
    assert_eq!(
        fs::read(target.join("_AUX/old_reports/_LPT3.txt").path()).unwrap(),
        b"verified file"
    );
    assert!(!folder.path().exists());
    assert_eq!(fs::read_dir(source.path()).unwrap().count(), 0);
}

#[test]
fn a_stale_source_or_new_destination_invalidates_the_entire_reviewed_plan() {
    for source_changed in [true, false] {
        let (_root, source, target) = folders();
        let first = source.join("first.txt");
        let second = source.join("second.txt");
        fs::write(first.path(), b"first").unwrap();
        fs::write(second.path(), b"second").unwrap();
        let plan = build(
            &[first.clone(), second.clone()],
            &target,
            TransferKind::Move,
            &mut Progress::default(),
        )
        .unwrap();
        if source_changed {
            fs::write(second.path(), b"new version").unwrap();
        } else {
            fs::write(target.join("SECOND.txt").path(), b"another writer").unwrap();
        }
        let failure = plan.apply(&mut Progress::default()).unwrap_err();
        assert!(failure.summary.preparing);
        assert_eq!(failure.summary.completed, 0);
        assert_eq!(failure.summary.remaining, [first.clone(), second.clone()]);
        let TransferError::Failed(Failure::Mutation(reason)) = failure.error else {
            panic!("Expected typed plan recheck failure: {failure:?}");
        };
        if source_changed {
            assert!(
                matches!(reason.as_ref(), MutationError::SourceChanged(path) if path == &second)
            );
        } else {
            assert!(
                matches!(reason.as_ref(), MutationError::CompatibleNameOccupied(path) if path == &target.join("second.txt"))
            );
        }
        assert_eq!(fs::read(first.path()).unwrap(), b"first");
        assert!(second.path().exists() && !target.join("first.txt").path().exists());
    }
}

#[test]
fn interruption_reports_the_completed_root_and_preserves_later_roots() {
    for stop in [true, false] {
        let (_root, source, target) = folders();
        let files = [
            source.join("a.txt"),
            source.join("b.txt"),
            source.join("c.txt"),
        ];
        for file in &files {
            fs::write(file.path(), file.name().as_bytes()).unwrap();
        }
        let plan = build(
            &files,
            &target,
            TransferKind::Copy,
            &mut Progress::default(),
        )
        .unwrap();
        let first = target.join("a.txt");
        let competitor = target.join("B.txt");
        let cancel = Cancellation::at_checkpoint({
            let (first, competitor) = (first.clone(), competitor.clone());
            move || {
                if !first.path().exists() {
                    return false;
                }
                if !stop && !competitor.path().exists() {
                    fs::write(competitor.path(), b"another writer").unwrap();
                }
                stop
            }
        });
        let mut progress = Progress::with_cancellation(cancel);
        let result = plan.apply(&mut progress);
        let (summary, interrupted) = if stop {
            (result.unwrap(), Outcome::Cancelled)
        } else {
            let failure = result.unwrap_err();
            assert!(
                matches!(failure.error, TransferError::Failed(Failure::Mutation(reason))
                if matches!(reason.as_ref(), MutationError::CompatibleNameOccupied(path) if path == &target.join("b.txt")))
            );
            assert_eq!(fs::read(competitor.path()).unwrap(), b"another writer");
            (failure.summary, Outcome::Failed)
        };
        assert_eq!(summary.completed, 1);
        assert!(!summary.preparing);
        assert_eq!(summary.cancelled, stop);
        assert_eq!(progress.results.cancelled, stop);
        assert_eq!(summary.remaining, files[1..]);
        let mut outcomes = progress.results.root_outcomes();
        outcomes.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            outcomes,
            [
                (files[0].clone(), Outcome::Completed),
                (files[1].clone(), interrupted),
                (files[2].clone(), Outcome::NotStarted),
            ]
        );
        assert_eq!(fs::read(first.path()).unwrap(), b"a.txt");
        for file in &files {
            assert_eq!(fs::read(file.path()).unwrap(), file.name().as_bytes());
        }
        assert_eq!(
            fs::read_dir(target.path()).unwrap().count(),
            if stop { 1 } else { 2 }
        );
    }
}

#[test]
fn stopping_a_direct_copy_preserves_the_partial_output_and_typed_cause() {
    let (_root, source, target) = folders();
    let original = source.join("data.txt");
    let destination = target.join("data.txt");
    fs::write(original.path(), b"original data").unwrap();
    let mut plan = build(
        std::slice::from_ref(&original),
        &target,
        TransferKind::Copy,
        &mut Progress::default(),
    )
    .unwrap();
    plan.direct = true;
    let output = destination.path().to_owned();
    let cancel = Cancellation::at_checkpoint(move || output.exists());
    let mut progress = Progress::with_cancellation(cancel);
    let failure = plan.apply(&mut progress).unwrap_err();
    assert_eq!(failure.summary.completed, 0);
    assert!(failure.summary.cancelled && progress.results.cancelled);
    assert_eq!(
        failure.summary.remaining.as_slice(),
        std::slice::from_ref(&original)
    );
    assert!(
        matches!(failure.error, TransferError::PublicationUnconfirmed(Failure::Publication(reason))
        if matches!(reason.as_ref(), PublicationError::CompatibleCopyUnconfirmed { target, cause: TransferError::Cancelled } if target == &destination))
    );
    assert_eq!(
        progress.results.root_outcomes(),
        [(original.clone(), Outcome::PublicationUnconfirmed)]
    );
    assert_eq!(fs::read(original.path()).unwrap(), b"original data");
    assert!(destination.path().is_file());
}

#[test]
fn a_verified_copy_with_original_remaining_is_not_a_completed_move() {
    let (_root, source, target) = folders();
    let original = source.join("report:final.txt");
    let destination = target.join("report_final.txt");
    fs::write(original.path(), b"original bytes").unwrap();
    let plan = build(
        std::slice::from_ref(&original),
        &target,
        TransferKind::Move,
        &mut Progress::default(),
    )
    .unwrap();
    let output = destination.path().to_owned();
    let cancel = Cancellation::at_checkpoint(move || output.exists());
    let mut progress = Progress::with_cancellation(cancel);
    let failure = plan.apply(&mut progress).unwrap_err();
    assert_eq!(failure.summary.completed, 0);
    assert!(failure.summary.cancelled && progress.results.cancelled);
    assert_eq!(
        failure.summary.remaining.as_slice(),
        std::slice::from_ref(&original)
    );
    assert!(
        matches!(failure.error, TransferError::Failed(Failure::Mutation(reason))
        if matches!(reason.as_ref(), MutationError::MoveIncomplete { target, cause: TransferError::Cancelled } if target == &destination))
    );
    assert_eq!(
        progress.results.root_outcomes(),
        [(original.clone(), Outcome::CopiedSourceRemaining)]
    );
    assert_eq!(fs::read(original.path()).unwrap(), b"original bytes");
    assert_eq!(fs::read(destination.path()).unwrap(), b"original bytes");
}
