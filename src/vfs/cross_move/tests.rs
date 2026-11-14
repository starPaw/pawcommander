use super::*;
use crate::result_history::Outcome;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

fn fixture(tree: bool) -> (tempfile::TempDir, TransferItem) {
    let root = tempfile::tempdir().unwrap();
    let source: Location = root.path().join("source").into();
    let target: Location = root.path().join("target").into();
    if tree {
        fs::create_dir(source.path()).unwrap();
        for name in ["a", "b", "c"] {
            fs::write(source.join(name).path(), name).unwrap();
        }
    } else {
        fs::write(source.path(), b"original payload").unwrap();
    }
    let item = TransferItem::new(source.clone(), source, target);
    (root, item)
}

fn progress(item: &TransferItem) -> Progress {
    let mut progress = Progress::default();
    progress
        .results
        .plan([(item.source.clone(), Some(item.target.display()))]);
    progress.results.begin(&item.source);
    progress
}

fn no_staging(root: &std::path::Path) {
    assert!(fs::read_dir(root).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".pawcommander-")));
}

#[test]
#[ignore = "requires two writable filesystems; see CONTRIBUTING.md"]
fn actual_cross_volume_routing_counts_only_removed_sources() {
    use crate::conflict::{Answer, Decision};
    use crate::fs_ops::TransferKind;
    use crate::vfs::transfer::transfer_run;
    use crate::vfs::{RetryTransfer, TransferSummary};
    let from = tempfile::tempdir_in(
        std::env::var_os("PAWCOMMANDER_TEST_MOVE_SOURCE_ROOT").expect("source volume"),
    )
    .unwrap();
    let to = tempfile::tempdir_in(
        std::env::var_os("PAWCOMMANDER_TEST_MOVE_TARGET_ROOT").expect("target volume"),
    )
    .unwrap();
    let source: Location = from.path().to_path_buf().into();
    let target: Location = to.path().to_path_buf().into();
    assert_eq!(
        metadata(&source)
            .unwrap()
            .same_local_volume(&metadata(&target).unwrap()),
        Some(false),
        "two different volumes required"
    );
    fs::write(source.join("first").path(), vec![35; 1024 * 1024]).unwrap();
    fs::write(target.join("first").path(), b"replace this old target").unwrap();
    fs::create_dir_all(source.join("tree/nested/empty").path()).unwrap();
    fs::write(
        source.join("tree/nested/data").path(),
        vec![37; 1024 * 1024],
    )
    .unwrap();
    fs::write(source.join("late").path(), vec![39; 1024 * 1024]).unwrap();
    let late_target = target.join("late");
    let cancel = Cancellation::at_checkpoint(move || late_target.path().exists());
    let mut progress = Progress::with_cancellation(cancel);
    progress.retry = Some(RetryTransfer::new(TransferKind::Move, target.clone()));
    let mut summary = TransferSummary::default();
    let mut consumed = Vec::new();
    let result = transfer_run(
        &[
            source.join("first"),
            source.join("tree"),
            source.join("late"),
        ],
        &target,
        TransferKind::Move,
        &mut progress,
        |_| {
            Ok(Answer {
                decision: Decision::Replace,
                apply_to_group: false,
            })
        },
        |path, _| consumed.push(path.clone()),
        &mut summary,
    );
    assert!(result.is_err());
    assert_eq!(summary.completed, 2);
    assert_eq!(summary.remaining, vec![source.join("late")]);
    assert_eq!(consumed, vec![source.join("first"), source.join("tree")]);
    assert!(!source.join("first").path().exists());
    assert!(!source.join("tree").path().exists());
    assert_eq!(
        fs::read(target.join("first").path()).unwrap(),
        vec![35; 1024 * 1024]
    );
    assert_eq!(
        fs::read(target.join("tree/nested/data").path()).unwrap(),
        vec![37; 1024 * 1024]
    );
    assert!(target.join("tree/nested/empty").path().is_dir());
    for path in [source.join("late"), target.join("late")] {
        assert_eq!(fs::read(path.path()).unwrap(), vec![39; 1024 * 1024]);
    }
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
            Outcome::Completed,
            Outcome::CopiedSourceRemaining
        ]
    );
    assert!(progress
        .retry
        .take()
        .unwrap()
        .unfinished(&progress.results)
        .is_none());
    assert_eq!(progress.undo.len(), 0);
    assert!(!progress.copy_verification());
    no_staging(from.path());
    no_staging(to.path());
}

#[test]
fn verifies_before_removal_and_preserves_nested_results() {
    let (root, item) = fixture(true);
    fs::create_dir_all(item.source.join("nested/empty").path()).unwrap();
    fs::write(item.source.join("nested/data").path(), vec![93; 180_000]).unwrap();
    let mut progress = progress(&item);
    let (mut published, mut removed) = (false, false);
    run(
        &item,
        None,
        &mut progress,
        || {
            assert!(item.source.path().exists());
            assert_eq!(
                fs::read(item.target.join("nested/data").path()).unwrap(),
                vec![93; 180_000]
            );
            published = true;
        },
        || {
            assert!(!item.source.path().exists());
            removed = true;
        },
        &mut |_, _| Ok(()),
    )
    .unwrap();
    assert!(published && removed);
    assert!(!progress.copy_verification());
    assert!(progress
        .results
        .items
        .iter()
        .filter(|row| row.parent.is_some())
        .all(|row| row.outcome == Outcome::Completed));
    assert_eq!(progress.undo.len(), 0);
    no_staging(root.path());
}

#[test]
fn hash_is_mandatory_even_when_copy_verification_is_off() {
    let (root, item) = fixture(false);
    let size = 256 * 1024;
    fs::write(item.source.path(), vec![17; size]).unwrap();
    fs::write(item.target.path(), b"existing target").unwrap();
    let expected = metadata(&item.target).unwrap();
    let mutated = Arc::new(AtomicBool::new(false));
    let seen = mutated.clone();
    let parent = root.path().to_path_buf();
    let cancel = Cancellation::at_checkpoint(move || {
        if !seen.load(Ordering::Relaxed) {
            for entry in fs::read_dir(&parent).unwrap().flatten() {
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pawcommander-copy-")
                {
                    continue;
                }
                let payload = entry.path().join("payload");
                if fs::metadata(&payload).is_ok_and(|meta| meta.len() == size as u64) {
                    use std::io::Write;
                    fs::OpenOptions::new()
                        .write(true)
                        .open(payload)
                        .unwrap()
                        .write_all(b"bad")
                        .unwrap();
                    seen.store(true, Ordering::Relaxed);
                }
            }
        }
        false
    });
    let mut progress = Progress::with_cancellation(cancel);
    let result = run(
        &item,
        Some(&expected),
        &mut progress,
        || panic!("bad copy published"),
        || panic!("source removed"),
        &mut |_, _| Ok(()),
    );
    assert!(mutated.load(Ordering::Relaxed));
    assert!(result.unwrap_err().to_string().contains("SHA-256"));
    assert_eq!(fs::read(item.source.path()).unwrap(), vec![17; size]);
    assert_eq!(fs::read(item.target.path()).unwrap(), b"existing target");
    assert!(!progress.copy_verification());
    no_staging(root.path());
}

#[test]
fn partial_removal_restores_remaining_tree_and_records_each_child() {
    let (root, item) = fixture(true);
    let mut progress = progress(&item);
    let error = run(
        &item,
        None,
        &mut progress,
        || {},
        || panic!("incomplete move"),
        &mut |path, phase| {
            if phase == Phase::Removed && path.path().file_name().unwrap() == "c" {
                return Err("injected removal failure".into());
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("injected removal failure"));
    assert!(!item.source.join("c").path().exists());
    for name in ["a", "b"] {
        assert_eq!(
            fs::read(item.source.join(name).path()).unwrap(),
            name.as_bytes()
        );
    }
    for name in ["a", "b", "c"] {
        assert_eq!(
            fs::read(item.target.join(name).path()).unwrap(),
            name.as_bytes()
        );
    }
    for row in progress
        .results
        .items
        .iter()
        .filter(|row| row.parent.is_some())
    {
        let is_c = std::path::Path::new(&row.source)
            .file_name()
            .is_some_and(|name| name == "c");
        assert_eq!(
            row.outcome,
            if is_c {
                Outcome::Completed
            } else {
                Outcome::CopiedSourceRemaining
            }
        );
    }
    no_staging(root.path());
}

#[test]
fn changes_or_stop_after_publication_never_remove_source() {
    for change in ["stop", "source", "target", "replacement", "failure"] {
        let (root, item) = fixture(false);
        let mut progress = progress(&item);
        let cancel = progress.cancellation();
        let result = run(
            &item,
            None,
            &mut progress,
            || {},
            || panic!("incomplete move"),
            &mut |_, phase| {
                match (change, phase) {
                    ("stop", Phase::Copied) => cancel.request(),
                    ("source", Phase::Copied) => {
                        fs::write(item.source.path(), b"changed source").unwrap()
                    }
                    ("target", Phase::BeforeRemove) => {
                        fs::write(item.target.path(), b"changed target").unwrap();
                    }
                    ("replacement", Phase::BeforeRemove) => {
                        fs::rename(item.target.path(), root.path().join("old-target")).unwrap();
                        fs::write(item.target.path(), b"original payload").unwrap();
                    }
                    ("failure", Phase::Quarantined) => return Err("injected failure".into()),
                    _ => {}
                }
                Ok(())
            },
        );
        assert!(result.is_err(), "{change}");
        assert_eq!(
            fs::read(item.source.path()).unwrap(),
            if change == "source" {
                b"changed source".as_slice()
            } else {
                b"original payload".as_slice()
            }
        );
        assert!(item.target.path().exists());
        assert_eq!(progress.results.cancelled, change == "stop");
        no_staging(root.path());
    }
}

#[test]
fn new_children_and_occupied_restore_names_are_preserved() {
    for occupied in [false, true] {
        let (root, item) = fixture(true);
        let mut held = None;
        let result = run(
            &item,
            None,
            &mut progress(&item),
            || {},
            || panic!("incomplete move"),
            &mut |path, phase| {
                if phase == Phase::Quarantined {
                    held = Some(path.clone());
                    fs::write(path.join("new-child").path(), b"external data").unwrap();
                    if occupied {
                        fs::write(item.source.path(), b"new original name").unwrap();
                    }
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        let remaining = if occupied {
            held.unwrap()
        } else {
            item.source.clone()
        };
        assert_eq!(
            fs::read(remaining.join("new-child").path()).unwrap(),
            b"external data"
        );
        assert_eq!(fs::read(remaining.join("a").path()).unwrap(), b"a");
        if occupied {
            assert_eq!(fs::read(item.source.path()).unwrap(), b"new original name");
            assert!(result
                .unwrap_err()
                .to_string()
                .contains(&remaining.display()));
        } else {
            no_staging(root.path());
        }
    }
}

#[test]
fn panic_does_not_clean_up_quarantined_source() {
    let (root, item) = fixture(false);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(
            &item,
            None,
            &mut progress(&item),
            || {},
            || {},
            &mut |_, phase| {
                if phase == Phase::Quarantined {
                    panic!("simulated worker panic");
                }
                Ok(())
            },
        )
    }));
    assert!(result.is_err());
    assert_eq!(fs::read(item.target.path()).unwrap(), b"original payload");
    let retained = fs::read_dir(root.path())
        .unwrap()
        .flatten()
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".pawcommander-transfer-")
        })
        .unwrap()
        .path()
        .join("source");
    assert_eq!(fs::read(retained).unwrap(), b"original payload");
}

#[cfg(unix)]
#[test]
fn local_links_are_not_followed_and_permissions_survive() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let (root, item) = fixture(true);
    let outside = root.path().join("outside");
    fs::write(&outside, b"untouched").unwrap();
    symlink(&outside, item.source.join("external-link").path()).unwrap();
    symlink("missing", item.source.join("broken-link").path()).unwrap();
    fs::set_permissions(
        item.source.join("a").path(),
        fs::Permissions::from_mode(0o400),
    )
    .unwrap();
    run(
        &item,
        None,
        &mut progress(&item),
        || {},
        || {},
        &mut |_, _| Ok(()),
    )
    .unwrap();
    assert!(!item.source.path().exists());
    assert_eq!(
        fs::read_link(item.target.join("external-link").path()).unwrap(),
        outside
    );
    assert_eq!(
        fs::read_link(item.target.join("broken-link").path()).unwrap(),
        PathBuf::from("missing")
    );
    assert_eq!(
        fs::metadata(item.target.join("a").path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o400
    );
    assert_eq!(fs::read(outside).unwrap(), b"untouched");
    no_staging(root.path());
}
