use super::*;

#[test]
fn conflict_group_replaces_files_and_asks_again_for_a_directory() {
    let root = tempfile::tempdir().unwrap();
    let root: Location = root.path().to_path_buf().into();
    let source = create_directory(&root, "source").unwrap();
    let target = create_directory(&root, "target").unwrap();
    let names = ["first.txt", "second.txt", "project.dir", "project (2).dir"];
    let sources: Vec<_> = names.iter().map(|name| source.join(name)).collect();
    for name in &names[..2] {
        fs::write(source.join(name).path(), "new bytes").unwrap();
        fs::write(target.join(name).path(), "old").unwrap();
    }
    fs::create_dir(source.join(names[2]).path()).unwrap();
    fs::create_dir(target.join(names[2]).path()).unwrap();
    fs::write(source.join(names[2]).join("nested.txt").path(), "nested").unwrap();
    fs::write(source.join(names[3]).path(), "reserved").unwrap();

    let mut prompted = Vec::new();
    let result = transfer_with_conflicts(
        &sources,
        &target,
        TransferKind::Copy,
        &mut Progress::default(),
        |conflict| {
            prompted.push(conflict.index);
            assert_eq!(conflict.count, 3);
            if conflict.index == 1 {
                assert!(conflict.can_replace);
                assert_eq!(conflict.source.size, Some(9));
                assert_eq!(conflict.target.size, Some(3));
                assert!(conflict.source.modified.is_some());
                assert!(conflict.target.modified.is_some());
                Ok(Answer {
                    decision: Decision::Replace,
                    apply_to_group: true,
                })
            } else {
                assert!(!conflict.can_replace);
                Ok(Answer {
                    decision: Decision::KeepBoth,
                    apply_to_group: false,
                })
            }
        },
        |_, _| panic!("copy must not report a move"),
    )
    .unwrap();
    assert_eq!(prompted, vec![1, 3]);
    assert_eq!(result.completed, 4);
    assert_eq!(result.skipped, 0);
    assert!(!result.cancelled);
    for name in &names[..2] {
        assert_eq!(fs::read(target.join(name).path()).unwrap(), b"new bytes");
    }
    assert_eq!(
        fs::read_dir(target.join(names[2]).path()).unwrap().count(),
        0
    );
    assert_eq!(
        fs::read(target.join("project (3).dir").join("nested.txt").path()).unwrap(),
        b"nested"
    );
    assert_eq!(fs::read(target.join(names[3]).path()).unwrap(), b"reserved");
}

#[test]
fn conflict_cancel_leaves_the_whole_batch_and_skip_preserves_cut_sources() {
    let root = tempfile::tempdir().unwrap();
    let root: Location = root.path().to_path_buf().into();
    let source = create_directory(&root, "source").unwrap();
    let target = create_directory(&root, "target").unwrap();
    let names = ["free.txt", "blocked.txt", "another.txt"];
    let sources: Vec<_> = names.iter().map(|name| source.join(name)).collect();
    for item in &sources {
        fs::write(item.path(), "source").unwrap();
    }
    for name in &names[1..] {
        fs::write(target.join(name).path(), "destination").unwrap();
    }
    let cancelled = transfer_with_conflicts(
        &sources,
        &target,
        TransferKind::Move,
        &mut Progress::default(),
        |_| {
            Ok(Answer {
                decision: Decision::Cancel,
                apply_to_group: false,
            })
        },
        |_, _| panic!("nothing may move before conflicts are resolved"),
    )
    .unwrap();
    assert!(cancelled.cancelled);
    assert!(sources.iter().all(|item| item.path().exists()));
    assert!(!target.join(names[0]).path().exists());

    let mut moved = Vec::new();
    let report = transfer_with_conflicts(
        &sources,
        &target,
        TransferKind::Move,
        &mut Progress::default(),
        |conflict| {
            assert_eq!(conflict.index, 1);
            Ok(Answer {
                decision: Decision::Skip,
                apply_to_group: true,
            })
        },
        |location, covered| {
            assert!(covered.is_empty());
            moved.push(location.clone());
        },
    )
    .unwrap();
    assert_eq!((report.completed, report.skipped), (1, 2));
    assert_eq!(moved, sources[..1]);
    assert!(!sources[0].path().exists());
    for (source, name) in sources[1..].iter().zip(&names[1..]) {
        assert_eq!(fs::read(source.path()).unwrap(), b"source");
        assert_eq!(fs::read(target.join(name).path()).unwrap(), b"destination");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn conflict_move_rejects_replaced_directory_and_keeps_its_selection_pending() {
    use std::os::unix::fs::symlink;

    for conflict_on_source in [true, false] {
        for replace_with_link in [true, false] {
            let fixture = tempfile::tempdir().unwrap();
            let root: Location = fixture.path().to_path_buf().into();
            let source = create_directory(&root, "project").unwrap();
            let child = source.join("child.txt");
            let original = root.join("original");
            let target = create_directory(&root, "target").unwrap();
            let mut sources = vec![source.clone(), child.clone()];
            let conflicting_source = if conflict_on_source {
                source.clone()
            } else {
                let other = root.join("other.txt");
                fs::write(other.path(), b"other source").unwrap();
                sources.push(other.clone());
                other
            };
            let existing = target.join(conflicting_source.path().file_name().unwrap());
            fs::write(child.path(), b"original child").unwrap();
            fs::write(existing.path(), b"existing target").unwrap();

            let error = transfer_with_conflicts(
                &sources,
                &target,
                TransferKind::Move,
                &mut Progress::default(),
                |conflict| {
                    assert_eq!(conflict.source.location, conflicting_source);
                    fs::rename(source.path(), original.path()).unwrap();
                    if replace_with_link {
                        symlink(original.path(), source.path()).unwrap();
                    } else {
                        fs::create_dir(source.path()).unwrap();
                        fs::write(child.path(), b"substitute").unwrap();
                    }
                    Ok(Answer {
                        decision: Decision::KeepBoth,
                        apply_to_group: false,
                    })
                },
                |_, _| panic!("the changed source must not retire any clipboard entries"),
            )
            .unwrap_err();
            assert!(crate::i18n::transfer_failure(&error).contains(&source.display()));
            assert_eq!(
                fs::read(original.join("child.txt").path()).unwrap(),
                b"original child"
            );
            if replace_with_link {
                assert_eq!(fs::read_link(source.path()).unwrap(), original.path());
            } else {
                assert_eq!(fs::read(child.path()).unwrap(), b"substitute");
            }
            assert_eq!(fs::read(existing.path()).unwrap(), b"existing target");
            assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1);
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn conflict_move_keeps_both_for_unchanged_directory_and_dangling_link() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let source = create_directory(&root, "project").unwrap();
    let child = source.join("child.txt");
    let link = root.join("link");
    let target = create_directory(&root, "target").unwrap();
    fs::write(child.path(), b"child").unwrap();
    symlink("missing", link.path()).unwrap();
    fs::write(target.join("project").path(), b"existing project").unwrap();
    fs::write(target.join("link").path(), b"existing link").unwrap();

    let mut moved = Vec::new();
    let report = transfer_with_conflicts(
        &[source.clone(), child.clone(), link.clone()],
        &target,
        TransferKind::Move,
        &mut Progress::default(),
        |conflict| {
            assert_eq!(conflict.source.location, source);
            Ok(Answer {
                decision: Decision::KeepBoth,
                apply_to_group: true,
            })
        },
        |source, covered| moved.push((source.clone(), covered.to_vec())),
    )
    .unwrap();
    assert_eq!(report.completed, 2);
    assert_eq!(
        moved,
        vec![
            (source.clone(), vec![child.path().into()]),
            (link.clone(), vec![])
        ]
    );
    assert!(!source.path().exists());
    assert!(fs::symlink_metadata(link.path()).is_err());
    assert_eq!(
        fs::read(target.join("project (2)").join("child.txt").path()).unwrap(),
        b"child"
    );
    assert_eq!(
        fs::read_link(target.join("link (2)").path()).unwrap(),
        Path::new("missing")
    );
    assert_eq!(
        fs::read(target.join("project").path()).unwrap(),
        b"existing project"
    );
    assert_eq!(
        fs::read(target.join("link").path()).unwrap(),
        b"existing link"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn conflict_move_moves_both_selected_hard_link_aliases() {
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let first = root.join("first.txt");
    let second = root.join("second.txt");
    let target = create_directory(&root, "target").unwrap();
    fs::write(first.path(), b"hard-linked content").unwrap();
    fs::hard_link(first.path(), second.path()).unwrap();
    fs::write(target.join("first.txt").path(), b"existing").unwrap();

    let mut moved = Vec::new();
    let report = transfer_with_conflicts(
        &[first.clone(), second.clone()],
        &target,
        TransferKind::Move,
        &mut Progress::default(),
        |_| {
            Ok(Answer {
                decision: Decision::KeepBoth,
                apply_to_group: false,
            })
        },
        |source, covered| {
            assert!(covered.is_empty());
            moved.push(source.clone());
        },
    )
    .unwrap();
    assert_eq!(report.completed, 2);
    assert!(!first.path().exists());
    assert!(!second.path().exists());
    assert_eq!(moved, [first, second]);
    let moved_first = target.join("first (2).txt");
    let moved_second = target.join("second.txt");
    let first_metadata = entry_metadata(&moved_first).unwrap().unwrap();
    let second_metadata = entry_metadata(&moved_second).unwrap().unwrap();
    assert!(first_metadata.same_object(&second_metadata));
    assert_eq!(
        fs::read(moved_first.path()).unwrap(),
        b"hard-linked content"
    );
    assert_eq!(
        fs::read(target.join("first.txt").path()).unwrap(),
        b"existing"
    );
}

#[test]
fn same_folder_move_stops_before_conflicts_but_copy_can_keep_both() {
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let destination = create_directory(&root, "destination").unwrap();
    let outside = root.join("outside.txt");
    let existing = destination.join("existing.txt");
    fs::write(outside.path(), b"outside").unwrap();
    fs::write(existing.path(), b"existing").unwrap();
    let reject_move = |destination: &Location| {
        let error = transfer_with_conflicts(
            &[outside.clone(), existing.clone()],
            destination,
            TransferKind::Move,
            &mut Progress::default(),
            |_| panic!("same-folder moves must fail before any conflict prompt"),
            |_, _| panic!("same-folder moves must fail before transferring any item"),
        )
        .unwrap_err();
        assert_eq!(
            crate::i18n::transfer_failure(&error),
            crate::i18n::text!("Элементы уже находятся в этой папке")
        );
        assert_eq!(fs::read(outside.path()).unwrap(), b"outside");
        assert_eq!(fs::read(existing.path()).unwrap(), b"existing");
        assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 1);
    };
    reject_move(&destination);
    #[cfg(unix)]
    {
        let alias = root.join("alias");
        std::os::unix::fs::symlink(destination.path(), alias.path()).unwrap();
        reject_move(&alias);
    }

    let report = transfer_with_conflicts(
        std::slice::from_ref(&existing),
        &destination,
        TransferKind::Copy,
        &mut Progress::default(),
        |conflict| {
            assert!(!conflict.can_replace);
            Ok(Answer {
                decision: Decision::KeepBoth,
                apply_to_group: false,
            })
        },
        |_, _| panic!("copy must not report a move"),
    )
    .unwrap();
    assert_eq!(report.completed, 1);
    assert_eq!(fs::read(existing.path()).unwrap(), b"existing");
    assert_eq!(
        fs::read(destination.join("existing (2).txt").path()).unwrap(),
        b"existing"
    );
}

#[test]
fn conflict_keeps_hard_link_aliases_and_creates_an_independent_copy() {
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let source = root.join("report.txt");
    let target_dir = create_directory(&root, "target").unwrap();
    let target = target_dir.join("report.txt");
    let copy = target_dir.join("report (2).txt");
    fs::write(source.path(), b"original").unwrap();
    fs::hard_link(source.path(), target.path()).unwrap();
    assert_ne!(canonical(&source).unwrap(), canonical(&target).unwrap());

    let report = transfer_with_conflicts(
        std::slice::from_ref(&source),
        &target_dir,
        TransferKind::Copy,
        &mut Progress::default(),
        |conflict| {
            assert!(!conflict.can_replace);
            Ok(Answer {
                decision: Decision::KeepBoth,
                apply_to_group: false,
            })
        },
        |_, _| panic!("copy must not report a move"),
    )
    .unwrap();
    assert_eq!(report.completed, 1);
    assert_eq!(fs::read(target.path()).unwrap(), b"original");
    fs::write(source.path(), b"updated").unwrap();
    assert_eq!(fs::read(target.path()).unwrap(), b"updated");
    assert_eq!(fs::read(copy.path()).unwrap(), b"original");
}

#[test]
fn conflict_replace_preserves_changed_destination_and_restores_on_publish_error() {
    let root = tempfile::tempdir().unwrap();
    let root: Location = root.path().to_path_buf().into();
    let target_dir = create_directory(&root, "target").unwrap();
    let target = target_dir.join("report.txt");
    let source = root.join("report.txt");
    fs::write(source.path(), "new").unwrap();
    fs::write(target.path(), "old").unwrap();
    let failure = transfer_with_conflicts(
        std::slice::from_ref(&source),
        &target_dir,
        TransferKind::Copy,
        &mut Progress::default(),
        |_| {
            fs::write(target.path(), "changed elsewhere").unwrap();
            Ok(Answer {
                decision: Decision::Replace,
                apply_to_group: false,
            })
        },
        |_, _| {},
    )
    .unwrap_err();
    assert!(
        crate::i18n::transfer_failure(&failure).contains(&crate::tr_format!(
            "Destination {target} changed after the conflict dialog was shown",
            target = target
        ))
    );
    assert_eq!(fs::read(target.path()).unwrap(), b"changed elsewhere");

    let expected = entry_metadata(&target).unwrap().unwrap();
    assert!(publish_item_typed(
        &root.join("missing"),
        &target,
        Some(&expected),
        |_| {},
        || {
            panic!("missing payload cannot be published");
        }
    )
    .is_err());
    assert_eq!(fs::read(target.path()).unwrap(), b"changed elsewhere");
    assert_eq!(fs::read_dir(target_dir.path()).unwrap().count(), 1);
}
