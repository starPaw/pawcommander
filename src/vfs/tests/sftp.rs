use super::*;
#[cfg(target_os = "macos")]
use crate::vfs::cache_media_preview;

fn check_sftp_retry(local_root: &Location, remote_root: &Location) {
    for (name, source_root, target_root, kind) in [
        ("upload", local_root, remote_root, TransferKind::Copy),
        ("download", remote_root, local_root, TransferKind::Copy),
        ("remote-copy", remote_root, remote_root, TransferKind::Copy),
        ("remote-move", remote_root, remote_root, TransferKind::Move),
        ("upload-move", local_root, remote_root, TransferKind::Move),
        ("download-move", remote_root, local_root, TransferKind::Move),
    ] {
        let source = source_root.join(format!("retry-{name}-source"));
        let target = target_root.join(format!("retry-{name}-target"));
        fs::create_dir_all(source.join("tree/empty").path()).unwrap();
        fs::create_dir(target.path()).unwrap();
        fs::write(source.join("a").path(), "completed").unwrap();
        fs::write(source.join("b").path(), "retry payload").unwrap();
        fs::write(source.join("tree/nested").path(), "nested payload").unwrap();
        fs::write(target.join("b").path(), "old destination").unwrap();
        let mut progress = Progress::default();
        progress.retry = Some(crate::vfs::RetryTransfer::new(kind, target.clone()));
        let summary = transfer_with_conflicts(
            &[source.join("a"), source.join("b"), source.join("tree")],
            &target,
            kind,
            &mut progress,
            |_| {
                Ok(Answer {
                    decision: Decision::Skip,
                    apply_to_group: false,
                })
            },
            |_, _| {},
        )
        .unwrap();
        assert_eq!((summary.completed, summary.skipped), (2, 1), "{name}");
        let retry = progress
            .retry
            .take()
            .unwrap()
            .unfinished(&progress.results)
            .unwrap();
        assert_eq!(retry.sources(), vec![source.join("b")]);
        fs::write(source.join("a").path(), "independent newer source").unwrap();
        let mut progress = Progress::default();
        progress.retry = Some(retry);
        let summary = transfer_with_conflicts(
            &[source.join("b")],
            &target,
            kind,
            &mut progress,
            |_| {
                Ok(Answer {
                    decision: Decision::KeepBoth,
                    apply_to_group: false,
                })
            },
            |_, _| {},
        )
        .unwrap();
        assert_eq!(summary.completed, 1, "{name}");
        assert!(progress
            .retry
            .take()
            .unwrap()
            .unfinished(&progress.results)
            .is_none());
        assert_eq!(fs::read(target.join("a").path()).unwrap(), b"completed");
        assert_eq!(
            fs::read(source.join("a").path()).unwrap(),
            b"independent newer source"
        );
        assert!(!target.join("a (2)").path().exists());
        assert_eq!(
            fs::read(target.join("b").path()).unwrap(),
            b"old destination"
        );
        assert_eq!(
            fs::read(target.join("b (2)").path()).unwrap(),
            b"retry payload"
        );
        assert_eq!(source.join("b").path().exists(), kind == TransferKind::Copy);
        assert_eq!(
            fs::read(target.join("tree/nested").path()).unwrap(),
            b"nested payload"
        );
        assert!(target.join("tree/empty").path().is_dir());
        assert!(!fs::read_dir(target.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".pawcommander")));
        fs::remove_dir_all(source.path()).unwrap();
        fs::remove_dir_all(target.path()).unwrap();
    }
}

#[cfg(unix)]
fn check_sftp_unsupported_names(local_root: &Location, remote_root: &Location) {
    for direction in ["upload", "download", "remote-copy"] {
        let (source_root, target_root) = match direction {
            "upload" => (local_root, remote_root),
            "download" => (remote_root, local_root),
            _ => (remote_root, remote_root),
        };
        for (index, name) in unsupported_sftp_names().into_iter().enumerate() {
            for late in [false, true] {
                let source =
                    source_root.join(format!("invalid-name-{direction}-{index}-{late}-source"));
                let target =
                    target_root.join(format!("invalid-name-{direction}-{index}-{late}-target"));
                fs::create_dir_all(source.join("nested").path()).unwrap();
                fs::write(source.join("nested/valid").path(), b"valid source").unwrap();
                let unsupported = source.join("nested").join(&name);
                if !late {
                    fs::write(unsupported.path(), b"unsupported source").unwrap();
                }
                fs::write(target.path(), b"previous destination").unwrap();
                let expected = entry_metadata(&target).unwrap().unwrap();
                let mut copied = false;
                let error = copy_transfer_checked(
                    &TransferItem::new(source.clone(), source.clone(), target.clone()),
                    Some(&expected),
                    &mut Progress::default(),
                    &mut |path, phase| {
                        assert!(late, "preflight must reject names before copying");
                        if path == &source && phase == CopyPhase::EntryCopied {
                            copied = true;
                            fs::write(unsupported.path(), b"unsupported source").unwrap();
                        }
                        Ok(())
                    },
                )
                .unwrap_err();
                assert_eq!(copied, late);
                // A directory metadata change may be observed before its
                // new unsupported name; either must abort and clean up.
                assert!(
                    error.contains("SFTP") || error.contains("changed during copying"),
                    "{error}"
                );
                assert!(
                    !error.contains("Осталась"),
                    "{direction}/{index}/{late}: {error}"
                );
                assert_eq!(fs::read(target.path()).unwrap(), b"previous destination");
                assert_eq!(fs::read(unsupported.path()).unwrap(), b"unsupported source");
                assert_eq!(
                    fs::read(source.join("nested/valid").path()).unwrap(),
                    b"valid source"
                );
                assert!(fs::read_dir(target_root.path()).unwrap().all(|entry| {
                    let name = entry.unwrap().file_name();
                    let name = name.to_string_lossy();
                    !name.starts_with(".pawcommander-sftp-")
                        && !name.starts_with(".pawcommander-transfer-")
                }));
                // The destination parent remains readable by the UI.
                read_directory(target_root, &|| false).unwrap();
            }
        }
    }
}

fn check_sftp_source_change_cleanup_at_depth_limit(local_root: &Location, remote_root: &Location) {
    for direction in ["upload", "remote-copy"] {
        let source_root = if direction == "upload" {
            local_root
        } else {
            remote_root
        };
        for depth in [128, 129] {
            let source = source_root.join(format!("deep-{direction}-{depth}-source"));
            let target = remote_root.join(format!("deep-{direction}-{depth}-target"));
            let sibling = remote_root.join(format!("deep-{direction}-{depth}-existing"));
            let mut leaf = source.clone();
            for _ in 1..depth {
                leaf = leaf.join("d");
            }
            fs::create_dir_all(leaf.path()).unwrap();
            leaf = leaf.join("file");
            fs::write(leaf.path(), b"original").unwrap();
            fs::write(sibling.path(), b"existing destination sibling").unwrap();
            let mut copied = false;
            let error = copy_transfer_checked(
                &TransferItem::new(source.clone(), source.clone(), target.clone()),
                None,
                &mut Progress::default(),
                &mut |path, phase| {
                    if path == &source && phase == CopyPhase::EntryCopied {
                        copied = true;
                        fs::write(leaf.path(), b"changed source").unwrap();
                    }
                    Ok(())
                },
            )
            .unwrap_err();
            assert_eq!(copied, depth == 128, "{direction}/{depth}: {error}");
            assert!(
                if copied {
                    error.contains("changed during copying")
                } else {
                    error.contains("Слишком большая глубина")
                },
                "{direction}/{depth}: {error}"
            );
            assert!(!error.contains("Осталась"), "{direction}/{depth}: {error}");
            assert!(!target.path().exists());
            assert_eq!(
                fs::read(sibling.path()).unwrap(),
                b"existing destination sibling"
            );
            assert_eq!(
                fs::read(leaf.path()).unwrap(),
                if copied {
                    b"changed source".as_slice()
                } else {
                    b"original"
                }
            );
            assert!(fs::read_dir(remote_root.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pawcommander-sftp-")
            }));
            if direction == "remote-copy" {
                // The extra staging allowance must not expand the depth
                // supported by ordinary user-requested remote deletion.
                let removed = trash_entries(std::slice::from_ref(&source));
                assert_eq!(removed.is_ok(), depth == 128);
                assert_eq!(leaf.path().exists(), depth == 129);
            }
        }
    }
}

// Uses only disposable loopback fixtures. Mutations are synchronized with
// copy checkpoints rather than sleeps or probabilistic background writers.
fn check_sftp_source_changes(local_root: &Location, remote_root: &Location) {
    for direction in ["upload", "download", "remote-copy"] {
        let (source_root, target_root) = match direction {
            "upload" => (local_root, remote_root),
            "download" => (remote_root, local_root),
            _ => (remote_root, remote_root),
        };
        for mutation in [
            "append",
            "truncate",
            "same-size-mtime",
            "opened-handle",
            "early-file",
            "add-child",
            "remove-child",
            "rename-child",
        ] {
            let source = source_root.join(format!("{direction}-{mutation}-source"));
            let target = target_root.join(format!("{direction}-{mutation}-target"));
            let tree = matches!(
                mutation,
                "early-file" | "add-child" | "remove-child" | "rename-child"
            );
            if tree {
                fs::create_dir_all(source.join("a-nested").path()).unwrap();
                fs::write(source.join("a-nested/early").path(), b"early").unwrap();
                fs::write(source.join("z-late").path(), b"late").unwrap();
            } else {
                fs::write(source.path(), b"original").unwrap();
            }
            fs::write(target.path(), b"previous destination").unwrap();
            let expected = entry_metadata(&target).unwrap().unwrap();
            let mut mutated = false;
            let error = copy_transfer_checked(
                &TransferItem::new(source.clone(), source.clone(), target.clone()),
                Some(&expected),
                &mut Progress::default(),
                &mut |path, phase| {
                    let checkpoint = if mutation == "opened-handle" {
                        CopyPhase::Opened
                    } else if tree {
                        CopyPhase::EntryCopied
                    } else {
                        CopyPhase::Read
                    };
                    let path_matches = if tree {
                        path == &source.join("z-late")
                    } else {
                        path == &source
                    };
                    if mutated || phase != checkpoint || !path_matches {
                        return Ok(());
                    }
                    mutated = true;
                    match mutation {
                        "append" => fs::write(source.path(), b"original grew").unwrap(),
                        "truncate" => fs::write(source.path(), b"short").unwrap(),
                        "same-size-mtime" => {
                            fs::write(source.path(), b"modified").unwrap();
                            fs::File::options()
                                .write(true)
                                .open(source.path())
                                .unwrap()
                                .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
                                .unwrap();
                        }
                        "opened-handle" => {
                            let metadata = fs::metadata(source.path()).unwrap();
                            let retained = source_root.join(format!("{direction}-retained-inode"));
                            fs::rename(source.path(), retained.path()).unwrap();
                            fs::write(source.path(), b"original").unwrap();
                            fs::File::options()
                                .write(true)
                                .open(source.path())
                                .unwrap()
                                .set_times(
                                    fs::FileTimes::new().set_modified(metadata.modified().unwrap()),
                                )
                                .unwrap();
                            // lstat now sees an indistinguishable SFTP file,
                            // but fstat must see the changed open inode.
                            fs::write(retained.path(), b"changed opened inode").unwrap();
                        }
                        "early-file" => {
                            fs::write(source.join("a-nested/early").path(), b"early changed")
                                .unwrap()
                        }
                        "add-child" => {
                            fs::write(source.join("a-nested/new").path(), b"new").unwrap()
                        }
                        "remove-child" => {
                            fs::remove_file(source.join("a-nested/early").path()).unwrap()
                        }
                        "rename-child" => fs::rename(
                            source.join("a-nested/early").path(),
                            source.join("a-nested/renamed").path(),
                        )
                        .unwrap(),
                        _ => unreachable!(),
                    }
                    Ok(())
                },
            )
            .unwrap_err();
            assert!(mutated, "{direction}/{mutation}");
            assert!(
                error.contains("changed during copying"),
                "{direction}/{mutation}: {error}"
            );
            assert_eq!(fs::read(target.path()).unwrap(), b"previous destination");
            assert!(source.path().exists());
            assert!(fs::read_dir(target_root.path()).unwrap().all(|entry| {
                let name = entry.unwrap().file_name();
                let name = name.to_string_lossy();
                !name.starts_with(".pawcommander-sftp-")
                    && !name.starts_with(".pawcommander-transfer-")
            }));
        }
    }
}

fn check_sftp_cancellation(local_root: &Location, remote_root: &Location) {
    let bytes = vec![0x6a; 3 * 64 * 1024];
    for (direction, from_root, to_root) in [
        ("upload", local_root, remote_root),
        ("download", remote_root, local_root),
        ("remote-copy", remote_root, remote_root),
    ] {
        for replace in [false, true] {
            let name = format!("cancel-{direction}-{replace}");
            let from = create_directory(from_root, &format!("{name}-from")).unwrap();
            let to = create_directory(to_root, &format!("{name}-to")).unwrap();
            let source = from.join("payload");
            let target = to.join("payload");
            fs::write(source.path(), &bytes).unwrap();
            if replace {
                fs::write(target.path(), b"previous target").unwrap();
            }
            let inspect = to.path().to_path_buf();
            let cancel = Cancellation::at_checkpoint(move || {
                fs::read_dir(&inspect)
                    .unwrap()
                    .filter_map(Result::ok)
                    .any(|entry| {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        (name.starts_with(".pawcommander-sftp-")
                            || name.starts_with(".pawcommander-transfer-"))
                            && fs::metadata(entry.path().join("payload"))
                                .is_ok_and(|meta| meta.len() > 0)
                    })
            });
            let mut progress = Progress::with_cancellation(cancel);
            let report = transfer_with_conflicts(
                std::slice::from_ref(&source),
                &to,
                TransferKind::Copy,
                &mut progress,
                |_| {
                    Ok(Answer {
                        decision: Decision::Replace,
                        apply_to_group: false,
                    })
                },
                |_, _| panic!("copy"),
            )
            .unwrap();
            assert!(report.cancelled && !report.preparing, "{name}");
            assert_eq!((report.completed, report.skipped), (0, 0), "{name}");
            assert_eq!(report.remaining, vec![source.clone()], "{name}");
            assert_eq!(fs::read(source.path()).unwrap(), bytes, "{name}");
            if replace {
                assert_eq!(
                    fs::read(target.path()).unwrap(),
                    b"previous target",
                    "{name}"
                );
            } else {
                assert!(!target.path().exists(), "{name}");
            }
            assert_eq!(
                fs::read_dir(to.path()).unwrap().count(),
                usize::from(replace),
                "{name}: staging removed"
            );
            eprintln!("cancellation {name}: source/target/remainder/cleanup passed");
        }
    }
    for count in [1, 2] {
        let from = create_directory(remote_root, &format!("cancel-move-{count}-from")).unwrap();
        let to = create_directory(remote_root, &format!("cancel-move-{count}-to")).unwrap();
        let sources: Vec<_> = (0..count)
            .map(|index| from.join(format!("file-{index}")))
            .collect();
        for source in &sources {
            fs::write(source.path(), &bytes).unwrap();
        }
        fs::write(to.join("file-0").path(), b"previous target").unwrap();
        let mut progress = Progress::default();
        let cancel = progress.cancellation();
        let mut moved = Vec::new();
        let report = transfer_with_conflicts(
            &sources,
            &to,
            TransferKind::Move,
            &mut progress,
            |_| {
                Ok(Answer {
                    decision: Decision::Replace,
                    apply_to_group: false,
                })
            },
            |source, _| {
                moved.push(source.clone());
                cancel.request();
            },
        )
        .unwrap();
        assert_eq!(report.completed, 1);
        assert_eq!(report.cancelled, count != 1);
        assert_eq!(report.remaining, sources[1..]);
        assert_eq!(moved, vec![sources[0].clone()]);
        assert!(!sources[0].path().exists());
        assert_eq!(fs::read(to.join("file-0").path()).unwrap(), bytes);
        assert_eq!(
            fs::read_dir(to.path()).unwrap().count(),
            1,
            "remote move cleanup"
        );
        for source in &sources[1..] {
            assert_eq!(fs::read(source.path()).unwrap(), bytes);
        }
        eprintln!(
            "cancellation remote move {count}: commit/event/remainder/late-stop/cleanup passed"
        );
    }
}

fn check_sftp_copy_hash_verification(local_root: &Location, remote_root: &Location) {
    use std::sync::atomic::{AtomicBool, Ordering};
    let local = create_directory(local_root, "hash-local").unwrap();
    let remote_dir = create_directory(remote_root, "hash-remote").unwrap();
    let bytes = vec![0x5a; 256 * 1024];
    for directory in [&local, &remote_dir] {
        fs::write(directory.join("source").path(), &bytes).unwrap();
    }
    for (name, source, parent) in [
        ("upload", local.join("source"), &remote_dir),
        ("download", remote_dir.join("source"), &local),
        ("remote", remote_dir.join("source"), &remote_dir),
    ] {
        for stop in [false, true] {
            let target = parent.join(format!("{name}-{stop}"));
            fs::write(target.path(), b"old destination").unwrap();
            let expected = entry_metadata(&target).unwrap().unwrap();
            let armed = Arc::new(AtomicBool::new(false));
            let watch = armed.clone();
            let mut checks = 0;
            let cancel = Cancellation::at_checkpoint(move || {
                if watch.load(Ordering::Relaxed) {
                    checks += 1;
                }
                // Verifying is armed immediately before readback: check1
                // precedes the first read, check2 follows that actual read.
                stop && checks == 2
            });
            let mut progress = Progress::with_cancellation(cancel);
            progress.set_copy_verification(true);
            let result = copy_transfer_cancellable(
                &TransferItem::new(source.clone(), source.clone(), target.clone()),
                Some(&expected),
                &mut progress,
                &mut |_, phase| {
                    if phase == CopyPhase::Verifying {
                        armed.store(true, Ordering::Relaxed);
                        if !stop {
                            let stage = fs::read_dir(parent.path())
                                .unwrap()
                                .map(Result::unwrap)
                                .find(|entry| {
                                    entry
                                        .file_name()
                                        .to_string_lossy()
                                        .starts_with(".pawcommander-")
                                })
                                .unwrap();
                            fs::write(stage.path().join("payload"), vec![0x42; bytes.len()])
                                .unwrap();
                        }
                    }
                    Ok(())
                },
                || panic!("failed verification must not publish"),
            );
            if stop {
                assert!(
                    matches!(result, Err(TransferError::Cancelled)),
                    "{name}: {result:?}"
                );
            } else {
                assert!(
                    result.unwrap_err().to_string().contains("SHA-256"),
                    "{name}"
                );
            }
            assert_eq!(fs::read(source.path()).unwrap(), bytes, "{name}: source");
            assert_eq!(
                fs::read(target.path()).unwrap(),
                b"old destination",
                "{name}: target"
            );
            assert!(
                fs::read_dir(parent.path())
                    .unwrap()
                    .map(Result::unwrap)
                    .all(|entry| !entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".pawcommander-")),
                "{name}: staging"
            );
        }
    }
}

fn check_cross_connection_partial_move(local_root: &Location, remote_root: &Location) {
    use crate::result_history::Outcome;
    for (name, from, to) in [
        ("upload", local_root, remote_root),
        ("download", remote_root, local_root),
    ] {
        let source = from.join(format!("partial-move-{name}-source"));
        let target = to.join(format!("partial-move-{name}-target"));
        fs::create_dir_all(source.path()).unwrap();
        fs::create_dir_all(target.path()).unwrap();
        fs::write(source.join("first").path(), b"first payload").unwrap();
        fs::write(source.join("late").path(), vec![64; 128 * 1024]).unwrap();
        let published_late = target.join("late");
        let cancel = Cancellation::at_checkpoint(move || published_late.path().exists());
        let mut progress = Progress::with_cancellation(cancel);
        progress.retry = Some(crate::vfs::RetryTransfer::new(
            TransferKind::Move,
            target.clone(),
        ));
        let mut summary = TransferSummary::default();
        let mut consumed = Vec::new();
        let result = super::super::transfer::transfer_run(
            &[source.join("first"), source.join("late")],
            &target,
            TransferKind::Move,
            &mut progress,
            |_| panic!("unexpected conflict"),
            |path, _| consumed.push(path.clone()),
            &mut summary,
        );
        assert!(result.is_err(), "{name}");
        assert_eq!(
            summary.completed, 1,
            "published copy is not a completed move"
        );
        assert_eq!(summary.remaining, vec![source.join("late")]);
        assert_eq!(consumed, vec![source.join("first")]);
        assert!(!source.join("first").path().exists());
        assert_eq!(
            fs::read(target.join("first").path()).unwrap(),
            b"first payload"
        );
        assert_eq!(
            fs::read(source.join("late").path()).unwrap(),
            vec![64; 128 * 1024]
        );
        assert_eq!(
            fs::read(target.join("late").path()).unwrap(),
            vec![64; 128 * 1024]
        );
        assert_eq!(
            progress
                .results
                .items
                .iter()
                .map(|row| row.outcome)
                .collect::<Vec<_>>(),
            vec![Outcome::Completed, Outcome::CopiedSourceRemaining]
        );
        assert!(progress
            .retry
            .take()
            .unwrap()
            .unfinished(&progress.results)
            .is_none());
        assert_eq!(progress.undo.len(), 0);
        assert!(progress.results.cancelled);
        assert!(fs::read_dir(source.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pawcommander-")));
    }
}

fn check_sftp_archive_round_trip(remote_root: &Location) {
    use crate::archive::{Format, Index};
    use crate::result_history::Outcome;
    use crate::vfs::{create_archive, edit_archive, extract_archive, ArchiveEdit};

    let parent = remote_root.join("archive-creation");
    let source = parent.join("Folder");
    fs::create_dir_all(source.join("Empty").path()).unwrap();
    let file = source.join("file.txt");
    fs::write(file.path(), b"SFTP archive contents").unwrap();
    for (format, name, password) in [
        (Format::Zip, "created.zip", None),
        (Format::Tar, "created.tar", None),
        (
            Format::Zip,
            "protected.zip",
            Some("disposable SFTP archive secret"),
        ),
    ] {
        let mut progress = Progress::default();
        create_archive(
            std::slice::from_ref(&source),
            &parent,
            name,
            format,
            password,
            &mut progress,
        )
        .unwrap();
        assert_eq!(progress.results.items[0].outcome, Outcome::Completed);
        // On this loopback fixture, an independent local read checks the server's file.
        let target = parent.join(name);
        let local_target = Location::Local(target.path().into());
        let location = local_target.browse_archive().unwrap().join("Folder");
        let index = Index::load(local_target.path(), &|| false).unwrap();
        let files = index.list(&location, &|| false).unwrap();
        assert!(files
            .iter()
            .any(|entry| entry.name == "Empty" && entry.is_dir));
        assert!(files
            .iter()
            .any(|entry| entry.name == "file.txt" && entry.size == 21));
        let created = fs::read(target.path()).unwrap();
        assert!(create_archive(
            std::slice::from_ref(&source),
            &parent,
            name,
            format,
            password,
            &mut progress
        )
        .is_err());
        assert_eq!(fs::read(target.path()).unwrap(), created);
        edit_archive(
            local_target.path(),
            ArchiveEdit::Add {
                folder: PathBuf::new(),
                sources: vec![file.clone()],
            },
            password,
            &mut progress,
        )
        .unwrap();
        let archive_root = local_target.browse_archive().unwrap();
        let extracted = parent.join(format!("extracted-{name}"));
        fs::create_dir(extracted.path()).unwrap();
        extract_archive(
            &[location, archive_root.join("file.txt")],
            &extracted,
            password,
            &mut progress,
            |_| Err("unexpected extraction conflict".into()),
        )
        .unwrap();
        assert!(extracted.join("Folder/Empty").path().is_dir());
        assert_eq!(
            fs::read(extracted.join("Folder/file.txt").path()).unwrap(),
            b"SFTP archive contents"
        );
        assert_eq!(
            fs::read(extracted.join("file.txt").path()).unwrap(),
            b"SFTP archive contents"
        );
        assert_eq!(fs::read_dir(extracted.path()).unwrap().count(), 2);
    }
    assert_eq!(fs::read(file.path()).unwrap(), b"SFTP archive contents");
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 7);
    fs::remove_dir_all(parent.path()).unwrap();
}

// Run with scripts/test-sftp.sh: a disposable loopback-only OpenSSH server
// and generated keys, without using the user's real known_hosts or keys.
#[test]
#[ignore = "requires disposable OpenSSH server; run scripts/test-sftp.sh"]
fn openssh_round_trip_and_host_verification() {
    let root =
        PathBuf::from(std::env::var_os("PAWCOMMANDER_TEST_SFTP_ROOT").expect("test server root"));
    let endpoint = Endpoint {
        host: std::env::var("PAWCOMMANDER_TEST_SFTP_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("PAWCOMMANDER_TEST_SFTP_PORT")
            .unwrap()
            .parse()
            .unwrap(),
        user: std::env::var("PAWCOMMANDER_TEST_SFTP_USER").unwrap(),
        key: Some(root.join("client")),
    };
    let known = root.join("known_hosts");
    let empty = root.join("empty_hosts");
    fs::write(&empty, "").unwrap();
    assert!(connect(&endpoint, "", "test-passphrase", &empty)
        .err()
        .unwrap()
        .contains("не подтверждён"));
    let mismatch = root.join("mismatch_hosts");
    let other_key = fs::read_to_string(root.join("other.pub")).unwrap();
    fs::write(
        &mismatch,
        format!("[{}]:{} {}", endpoint.host, endpoint.port, other_key),
    )
    .unwrap();
    assert!(connect(&endpoint, "", "test-passphrase", &mismatch)
        .err()
        .unwrap()
        .contains("изменился"));
    assert!(connect(&endpoint, "", "wrong-passphrase", &known).is_err());
    let memory_key = fs::read_to_string(root.join("client")).unwrap();
    let absent_key = Endpoint {
        key: Some(root.join("does-not-exist")),
        ..endpoint.clone()
    };
    assert!(connect_with_key(
        &absent_key,
        "",
        "test-passphrase",
        &known,
        Some(&memory_key)
    )
    .unwrap()
    .authenticated());
    assert!(connect_with_key(&absent_key, "", "wrong-phrase", &known, Some(&memory_key)).is_err());
    let remote_root = Location::Sftp {
        endpoint: endpoint.clone(),
        path: root.join("remote"),
    };
    assert!(read_automatic_directory(&remote_root, &|| false)
        .unwrap()
        .is_none());
    visit_address_directory(&remote_root, true, &|| false, |_| {
        panic!("completion must not authenticate to browse an unknown endpoint")
    })
    .unwrap();
    assert!(!sessions().lock().unwrap().contains_key(&endpoint));
    let session = connect(&endpoint, "", "test-passphrase", &known).unwrap();
    sessions().lock().unwrap().insert(endpoint.clone(), session);
    let local_root = Location::Local(root.join("local"));
    fs::create_dir(local_root.path()).unwrap();
    fs::create_dir(remote_root.path()).unwrap();
    super::undo::sftp_undo(&remote_root);
    check_sftp_archive_round_trip(&remote_root);
    let refresh_file = remote_root.join("external.txt");
    fs::write(refresh_file.path(), b"external edit").unwrap();
    let accessed = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    let modified = UNIX_EPOCH + Duration::from_secs(1_650_000_000);
    fs::OpenOptions::new()
        .write(true)
        .open(refresh_file.path())
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_accessed(accessed)
                .set_modified(modified),
        )
        .unwrap();
    #[cfg(unix)]
    let expected_owner = {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        fs::set_permissions(refresh_file.path(), fs::Permissions::from_mode(0o640)).unwrap();
        fs::metadata(refresh_file.path()).unwrap().uid()
    };
    let entries = read_automatic_directory(&remote_root, &|| false)
        .unwrap()
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, refresh_file);
    assert_eq!(entries[0].size, 13);
    assert_eq!(entries[0].modified, Some(modified));
    assert_eq!(entries[0].metadata.accessed, Some(accessed));
    assert_eq!(entries[0].metadata.created, None);
    #[cfg(unix)]
    {
        assert_eq!(entries[0].metadata.mode, Some(0o640));
        assert_eq!(entries[0].metadata.owner_id, Some(expected_owner));
        let alias = remote_root.with_path(root.join("remote-alias"));
        std::os::unix::fs::symlink(remote_root.path(), alias.path()).unwrap();
        let error = transfer_with_conflicts(
            std::slice::from_ref(&refresh_file),
            &alias,
            TransferKind::Move,
            &mut Progress::default(),
            |_| panic!("same-folder moves through an alias must fail before conflicts"),
            |_, _| panic!("same-folder moves must preserve the original entry"),
        )
        .unwrap_err();
        assert_eq!(
            error,
            crate::i18n::text("Элементы уже находятся в этой папке")
        );
        assert_eq!(fs::read(refresh_file.path()).unwrap(), b"external edit");
    }
    fs::remove_file(refresh_file.path()).unwrap();
    assert!(read_automatic_directory(&remote_root, &|| false)
        .unwrap()
        .unwrap()
        .is_empty());
    let completion_folder = remote_root.join("Žlutá папка #📂");
    fs::create_dir(completion_folder.path()).unwrap();
    let actual = fs::read_dir(remote_root.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let completion_folder = remote_root.with_path(actual.path());
    let mut completion = crate::address::Completion::default();
    completion.edited = true;
    let query = format!("{}/z\u{30c}l", remote_root.display());
    let ctx = eframe::egui::Context::default();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let suggestions = completion.suggestions(&query, &local_root, &ctx);
        if let Some(suggestion) = suggestions.first() {
            assert_eq!(suggestion.path, completion_folder);
            assert_eq!(suggestion.name, actual.file_name().to_str().unwrap());
            completion.remember_completion(&suggestion.path);
            let tab_address = format!("{}/", suggestion.path.display());
            assert_eq!(
                completion.resolve(&tab_address, &local_root).unwrap(),
                completion_folder
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "expected SFTP completion"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    fs::write(refresh_file.path(), b"another entry").unwrap();
    let mut tree_children = Vec::new();
    visit_connected_directory(&remote_root, &|| false, |entry| {
        if entry.is_dir {
            tree_children.push(entry.path);
        }
    })
    .unwrap();
    assert_eq!(tree_children, std::slice::from_ref(&completion_folder));
    let other_profile = Location::Sftp {
        endpoint: absent_key,
        path: remote_root.path().into(),
    };
    assert!(visit_connected_directory(&other_profile, &|| false, |_| {
        panic!("the tree must use the exact SFTP profile")
    })
    .is_err());
    let visited = std::cell::Cell::new(0);
    visit_address_directory(&remote_root, false, &|| visited.get() == 1, |entry| {
        assert!(entry.path == completion_folder || entry.path == refresh_file);
        visited.set(visited.get() + 1);
    })
    .unwrap();
    assert_eq!(visited.get(), 1);
    fs::remove_file(refresh_file.path()).unwrap();
    fs::remove_dir(completion_folder.path()).unwrap();
    let source = create_directory(&local_root, "папка ' [test]").unwrap();
    fs::write(source.path().join("hello world.txt"), "Привет, SFTP!\n").unwrap();
    fs::write(source.path().join(".hidden"), "hidden").unwrap();
    fs::write(source.path().join("empty"), "").unwrap();
    fs::create_dir(source.path().join("sub")).unwrap();
    fs::write(source.path().join("sub/large.bin"), vec![0xab; 512 * 1024]).unwrap();
    // A stopped worker reports its own remote staging path without scanning
    // or deleting unrelated SFTP entries.
    let recovery_dir = local_root.path().join("recovery");
    let pending = crate::recovery::begin(&recovery_dir, "upload", "fixture", 1).unwrap();
    let mut interrupted_staging = None;
    let interrupted = pending
        .run(|| {
            with_staging(&remote_root.join("unpublished"), |staging, preserve| {
                *preserve = true;
                fs::write(staging.path().join("payload"), b"partial upload").unwrap();
                interrupted_staging = Some(staging.clone());
                Err("injected interruption".into())
            })
        })
        .unwrap();
    assert!(interrupted.is_err());
    drop(pending);
    let report = crate::recovery::scan(&recovery_dir);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.unfinished.len(), 1);
    assert_eq!(report.unfinished[0].staging.len(), 1);
    assert!(report.unfinished[0].planned_staging.is_empty());
    let staged = interrupted_staging.unwrap();
    assert_eq!(report.unfinished[0].staging[0].location, staged.display());
    assert!(report.unfinished[0].staging[0].identity.is_none());
    assert_eq!(
        fs::read(staged.path().join("payload")).unwrap(),
        b"partial upload"
    );
    fs::remove_dir_all(staged.path()).unwrap();
    if std::env::var("PAWCOMMANDER_TEST_DISABLE_STATVFS").as_deref() == Ok("1") {
        let target = remote_root.join(source.name());
        let error = staged_copy(&source, &target).unwrap_err();
        assert!(error.contains("fstatvfs@openssh.com"), "{error}");
        assert!(!target.path().exists());
        assert!(read_directory(&remote_root, &|| false).unwrap().is_empty());
        assert_eq!(fs::read(source.path().join(".hidden")).unwrap(), b"hidden");
        let mut move_progress = Progress::default();
        let error = transfer_with_conflicts(
            std::slice::from_ref(&source),
            &remote_root,
            TransferKind::Move,
            &mut move_progress,
            |_| panic!("unexpected conflict"),
            |_, _| panic!("source removed"),
        )
        .unwrap_err();
        assert!(error.contains("fstatvfs@openssh.com"), "{error}");
        assert!(!target.path().exists());
        assert_eq!(fs::read(source.path().join(".hidden")).unwrap(), b"hidden");
        assert!(read_directory(&remote_root, &|| false).unwrap().is_empty());
        sessions().lock().unwrap().remove(&endpoint);
        return;
    }
    // Exercise fstatvfs on the destination filesystem before upload,
    // then exercise the remote size walk used before download/replacement.
    let expected_size = 512 * 1024 + "Привет, SFTP!\n".len() as u64 + "hidden".len() as u64;
    assert_eq!(
        TransferSourceManifest::capture(&source).unwrap().bytes,
        expected_size
    );
    with_staging(&remote_root.join(source.name()), |staging, _| {
        check_copy_space(expected_size, &remote_root.join(source.name()), staging)
    })
    .unwrap();
    let (mut progress, mut view) = Progress::tracked();
    progress.set_copy_verification(true);
    assert_eq!(
        transfer_with_conflicts(
            std::slice::from_ref(&source),
            &remote_root,
            TransferKind::Copy,
            &mut progress,
            |_| panic!("unexpected conflict"),
            |_, _| {},
        )
        .unwrap()
        .completed,
        1
    );
    assert_eq!(
        view.snapshot().total_written,
        512 * 1024 + "Привет, SFTP!\n".len() as u64 + "hidden".len() as u64
    );
    let entries = read_directory(&remote_root, &|| false).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, source.name());
    assert!(entries[0].is_dir);
    let uploaded = entries[0].path.clone();
    assert_eq!(
        TransferSourceManifest::capture(&uploaded).unwrap().bytes,
        expected_size
    );
    check_copy_space(
        expected_size,
        &local_root.join("download-preflight"),
        &local_root,
    )
    .unwrap();
    assert_eq!(read_directory(&uploaded, &|| false).unwrap().len(), 4);
    assert!(transfer(
        std::slice::from_ref(&source),
        &remote_root,
        TransferKind::Copy
    )
    .is_err());
    let downloads = create_directory(&local_root, "downloads").unwrap();
    let mut verified_download = Progress::default();
    verified_download.set_copy_verification(true);
    transfer_with_conflicts(
        std::slice::from_ref(&uploaded),
        &downloads,
        TransferKind::Copy,
        &mut verified_download,
        |_| panic!("unexpected conflict"),
        |_, _| panic!("copy"),
    )
    .unwrap();
    assert_eq!(
        fs::read(downloads.join(source.name()).path().join("sub/large.bin")).unwrap(),
        vec![0xab; 512 * 1024]
    );
    assert_eq!(
        fs::read_to_string(downloads.join(source.name()).path().join("hello world.txt")).unwrap(),
        "Привет, SFTP!\n"
    );
    let file_source = source.join("hello world.txt");
    let replace = |conflict: Conflict| {
        assert!(conflict.can_replace);
        assert!(conflict.source.size.is_some());
        assert!(conflict.target.modified.is_some());
        Ok(Answer {
            decision: Decision::Replace,
            apply_to_group: false,
        })
    };
    transfer_with_conflicts(
        std::slice::from_ref(&file_source),
        &uploaded,
        TransferKind::Copy,
        &mut Progress::default(),
        replace,
        |_, _| {},
    )
    .unwrap();
    assert_eq!(
        preview_data(&uploaded.join("hello world.txt"), 5)
            .unwrap()
            .1
            .len(),
        6
    );
    assert_eq!(
        preview_data(&uploaded.join("hello world.txt"), 1024)
            .unwrap()
            .1,
        "Привет, SFTP!\n".as_bytes()
    );
    {
        use std::io::{Read, Seek, SeekFrom};
        let location = uploaded.join("hello world.txt");
        let mut file = crate::vfs::open_preview_stream(&location).unwrap();
        file.seek(SeekFrom::Start("Привет, ".len() as u64)).unwrap();
        let mut tail = String::new();
        file.read_to_string(&mut tail).unwrap();
        assert_eq!(tail, "SFTP!\n");
    }
    {
        let text = "ordinary remote line\r\n".repeat(4096) + "REMOTE PREVIEW MARKER\r\n";
        let mut utf16 = vec![0xff, 0xfe];
        utf16.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        for (name, bytes) in [
            ("large UTF-8 preview.txt", text.into_bytes()),
            ("large UTF-16 preview.txt", utf16),
        ] {
            let location = uploaded.join(name);
            fs::write(location.path(), &bytes).unwrap();
            crate::preview::check_sftp_large_text(&location);
            assert_eq!(fs::read(location.path()).unwrap(), bytes);
            fs::remove_file(location.path()).unwrap();
        }
    }
    {
        let location = uploaded.join("binary preview.bin");
        let marker = b"\xde\xad\xbe\xef";
        let offset = 65535;
        let mut bytes = vec![0; 131_077];
        bytes[offset..offset + marker.len()].copy_from_slice(marker);
        fs::write(location.path(), &bytes).unwrap();
        crate::preview::check_sftp_hex(&location, offset as u64, marker);
        assert_eq!(fs::read(location.path()).unwrap(), bytes);
        fs::remove_file(location.path()).unwrap();
    }
    #[cfg(target_os = "macos")]
    {
        let location = uploaded.join("hello world.txt");
        let mut destination = Vec::new();
        let cancel = crate::operations::Cancellation::default();
        assert!(cache_media_preview(&location, &mut destination, 5, &cancel).is_err());
        assert!(destination.is_empty());
        cache_media_preview(&location, &mut destination, 1024, &cancel).unwrap();
        assert_eq!(destination, "Привет, SFTP!\n".as_bytes());
        let mut checks = 0;
        let cancel = crate::operations::Cancellation::at_checkpoint(move || {
            checks += 1;
            checks == 3
        });
        destination.clear();
        assert!(cache_media_preview(&location, &mut destination, 1024, &cancel).is_err());
        assert!(destination.is_empty());
    }
    fs::write(file_source.path(), "replacement").unwrap();
    transfer_with_conflicts(
        std::slice::from_ref(&file_source),
        &uploaded,
        TransferKind::Copy,
        &mut Progress::default(),
        replace,
        |_, _| {},
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(uploaded.join("hello world.txt").path()).unwrap(),
        "replacement"
    );
    transfer_with_conflicts(
        std::slice::from_ref(&file_source),
        &uploaded,
        TransferKind::Copy,
        &mut Progress::default(),
        |_| {
            Ok(Answer {
                decision: Decision::KeepBoth,
                apply_to_group: false,
            })
        },
        |_, _| {},
    )
    .unwrap();
    let remote_source = uploaded.join("hello world (2).txt");
    let remote_target = remote_root.join("hello world (2).txt");
    assert_eq!(fs::read(remote_source.path()).unwrap(), b"replacement");
    fs::write(remote_target.path(), "previous remote file").unwrap();
    transfer_with_conflicts(
        std::slice::from_ref(&remote_source),
        &remote_root,
        TransferKind::Move,
        &mut Progress::default(),
        replace,
        |_, _| {},
    )
    .unwrap();
    assert!(!remote_source.path().exists());
    assert_eq!(fs::read(remote_target.path()).unwrap(), b"replacement");
    assert!(transfer_with_conflicts(
        std::slice::from_ref(&source),
        &remote_root,
        TransferKind::Copy,
        &mut Progress::default(),
        |conflict| {
            assert!(!conflict.can_replace);
            Ok(Answer {
                decision: Decision::Replace,
                apply_to_group: false,
            })
        },
        |_, _| {},
    )
    .is_err());
    let disposable = create_directory(&remote_root, "delete-me").unwrap();
    fs::write(disposable.path().join("file"), "delete").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(source.path(), disposable.path().join("link")).unwrap();
    fs::create_dir(disposable.path().join("nested")).unwrap();
    fs::write(disposable.path().join("nested/child"), "nested delete").unwrap();
    let mut deletion = Progress::default();
    trash_entries_with_progress(std::slice::from_ref(&disposable), &mut deletion).unwrap();
    let record = deletion
        .results
        .finish("delete".into(), 1, &Ok("removed".into()))
        .unwrap();
    assert!(record.items[0].descendants_enumerated);
    assert_eq!(record.items.len(), if cfg!(unix) { 5 } else { 4 });
    assert!(record
        .items
        .iter()
        .all(|row| row.outcome == crate::result_history::Outcome::Completed));
    assert!(record
        .items
        .iter()
        .any(|row| row.source == disposable.join("nested/child").display()));
    assert!(!disposable.path().exists());
    assert!(source.path().exists());
    let renamed = rename_entry(&uploaded, "renamed").unwrap();
    let conflict = create_directory(&remote_root, "conflict").unwrap();
    assert!(rename_entry(&renamed, "conflict").is_err());
    assert!(remote(&conflict)
        .unwrap()
        .stat(conflict.path())
        .unwrap()
        .is_dir());
    // Direct no-replace publication must also reject an existing target.
    let file = renamed.join("hello world.txt");
    assert!(staged_copy(&file, &renamed.join(".hidden")).is_err());
    let ambiguous = super::super::transfer::rename_for_publication(&file, &renamed.join(".hidden"));
    assert!(matches!(
        ambiguous,
        Err(TransferError::PublicationUnconfirmed(_))
    ));
    assert_eq!(fs::read_to_string(file.path()).unwrap(), "replacement");
    assert_eq!(
        fs::read_to_string(renamed.path().join(".hidden")).unwrap(),
        "hidden"
    );
    let sub = create_directory(&remote_root, "destination").unwrap();
    transfer(std::slice::from_ref(&renamed), &sub, TransferKind::Move).unwrap();
    let moved = sub.join("renamed");
    assert!(transfer(std::slice::from_ref(&moved), &moved, TransferKind::Copy).is_err());
    assert!(remote(&moved).unwrap().stat(moved.path()).unwrap().is_dir());
    // Remote -> remote copy and refusal to follow symlinks in transfer trees.
    let mut verified_remote = Progress::default();
    verified_remote.set_copy_verification(true);
    transfer_with_conflicts(
        std::slice::from_ref(&moved),
        &conflict,
        TransferKind::Copy,
        &mut verified_remote,
        |_| panic!("unexpected conflict"),
        |_, _| panic!("copy"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source.path(), remote_root.path().join("alias")).unwrap();
        let link = remote_root.join("alias");
        assert!(read_directory(&remote_root, &|| false)
            .unwrap()
            .iter()
            .any(|e| e.is_symlink && e.is_dir));
        assert!(transfer(&[link], &downloads, TransferKind::Copy).is_err());
        assert!(!downloads.path().join("alias").exists());
        std::os::unix::fs::symlink(source.path(), source.path().join("loop")).unwrap();
        assert!(transfer(
            std::slice::from_ref(&source),
            &remote_root,
            TransferKind::Copy
        )
        .is_err());
        assert!(!remote_root.join(source.name()).path().exists());
    }
    assert!(fs::read_dir(remote_root.path()).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pawcommander-sftp-")
    }));
    assert!(fs::read_dir(downloads.path()).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pawcommander-transfer-")
    }));
    check_sftp_cancellation(&local_root, &remote_root);
    check_sftp_retry(&local_root, &remote_root);
    check_cross_connection_partial_move(&local_root, &remote_root);
    check_sftp_copy_hash_verification(&local_root, &remote_root);
    check_sftp_source_changes(&local_root, &remote_root);
    check_sftp_source_change_cleanup_at_depth_limit(&local_root, &remote_root);
    #[cfg(unix)]
    check_sftp_unsupported_names(&local_root, &remote_root);
    sessions().lock().unwrap().remove(&endpoint);
}
