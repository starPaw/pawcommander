use super::*;

#[test]
fn staging_cleanup_preserves_a_replacement_directory() {
    let fixture = tempfile::tempdir().unwrap();
    let target: Location = fixture.path().join("target").into();
    let original = fixture.path().join("original-staging");
    let mut replacement = None;
    let error = with_staging(&target, |staging, _| {
        fs::rename(staging.path(), &original).unwrap();
        fs::create_dir(staging.path()).unwrap();
        fs::write(staging.path().join("foreign"), b"preserve").unwrap();
        replacement = Some(staging.path().to_path_buf());
        Err("injected failure".into())
    })
    .unwrap_err();
    assert!(error.contains("injected failure"));
    assert!(error.contains("identity changed"));
    assert_eq!(
        fs::read(replacement.unwrap().join("foreign")).unwrap(),
        b"preserve"
    );
    assert!(original.is_dir());
}

#[test]
fn local_rename_denial_is_definite_and_preserves_both_files() {
    let fixture = tempfile::tempdir().unwrap();
    let source: Location = fixture.path().join("source").into();
    let target: Location = fixture.path().join("target").into();
    fs::write(source.path(), b"source").unwrap();
    fs::write(target.path(), b"old target").unwrap();

    let result = super::super::transfer::rename_for_publication(&source, &target);
    assert!(matches!(result, Err(TransferError::Failed(_))));
    assert_eq!(fs::read(source.path()).unwrap(), b"source");
    assert_eq!(fs::read(target.path()).unwrap(), b"old target");
}

#[test]
fn unknown_publication_survives_staging_cleanup_failure() {
    let staging: Location = PathBuf::from("/private-staging").into();
    let target: Location = PathBuf::from("/destination").into();
    let result = staging_result_typed(
        Err(TransferError::PublicationUnconfirmed("reply lost".into())),
        Err("cleanup refused".into()),
        &staging,
        &target,
    );
    match result {
        Err(TransferError::PublicationUnconfirmed(message)) => {
            assert!(message.contains("reply lost"));
            assert!(message.contains("cleanup refused"));
            assert!(message.contains("private-staging"));
        }
        other => panic!("uncertain publication became retryable: {other:?}"),
    }
}

#[test]
fn failed_initial_backup_rename_retains_original_destination_and_cleans_staging() {
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let source = root.join("report.txt");
    let target_dir = create_directory(&root, "target").unwrap();
    let target = target_dir.join("report.txt");
    fs::write(source.path(), b"new source payload").unwrap();
    fs::write(target.path(), b"previous destination").unwrap();
    let expected = entry_metadata(&target).unwrap().unwrap();
    let mut backup_path = None;
    let mut rename_calls = 0;

    let error = fs_ops::copy_atomically_with_progress(
        source.path(),
        target.path(),
        &mut Progress::default(),
        |payload, _| {
            assert_eq!(fs::read(payload).unwrap(), b"new source payload");
            with_staging(&target, |staging, preserve| {
                publish_replacement_with_backup_rename(
                    &payload.to_path_buf().into(),
                    &target,
                    staging,
                    &expected,
                    preserve,
                    &mut false,
                    |from, to| {
                        rename_calls += 1;
                        assert_eq!(from, &target);
                        assert_eq!(to, &staging.join("previous"));
                        backup_path = Some(to.clone());
                        Err("injected first backup rename denial".into())
                    },
                )
            })
        },
    )
    .unwrap_err();

    assert_eq!(rename_calls, 1);
    let backup_path = backup_path.unwrap();
    assert!(!backup_path.path().exists());
    assert!(!backup_path.parent().unwrap().path().exists());
    assert_eq!(fs::read(source.path()).unwrap(), b"new source payload");
    assert_eq!(fs::read(target.path()).unwrap(), b"previous destination");
    assert_eq!(fs::read_dir(target_dir.path()).unwrap().count(), 1);
    assert!(error.starts_with("injected first backup rename denial"));
    assert!(error.contains(&target.display().to_string()), "{error}");
    assert!(
        !error.contains(&backup_path.display().to_string()),
        "guidance must not direct the user to a removed backup: {error}"
    );
}

#[test]
fn replacement_rollback_preserves_backup_when_target_is_recreated() {
    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let source = root.join("report.txt");
    let target_dir = create_directory(&root, "target").unwrap();
    let target = target_dir.join("report.txt");
    fs::write(source.path(), b"new source payload").unwrap();
    fs::write(target.path(), b"previous destination").unwrap();
    let expected = entry_metadata(&target).unwrap().unwrap();
    let mut backup_path = None;

    let error = fs_ops::copy_atomically_with_progress(
        source.path(),
        target.path(),
        &mut Progress::default(),
        |payload, _| {
            with_staging(&target, |staging, preserve| {
                let result = publish_replacement_with_backup_rename(
                    &payload.to_path_buf().into(),
                    &target,
                    staging,
                    &expected,
                    preserve,
                    &mut false,
                    |from, to| {
                        rename_without_replacing(from, to)?;
                        backup_path = Some(to.clone());
                        // Another writer wins the now-vacant pathname. Both
                        // publication and rollback must refuse to overwrite it.
                        fs::write(target.path(), b"external writer").unwrap();
                        Ok(())
                    },
                );
                assert!(result.is_err());
                assert!(*preserve, "rollback failure must retain the old version");
                result
            })
        },
    )
    .unwrap_err();

    let backup = backup_path.unwrap();
    assert_eq!(fs::read(source.path()).unwrap(), b"new source payload");
    assert_eq!(fs::read(target.path()).unwrap(), b"external writer");
    assert_eq!(fs::read(backup.path()).unwrap(), b"previous destination");
    assert!(backup.parent().unwrap().path().is_dir());
    assert!(error.contains(&backup.display().to_string()), "{error}");
    // Only the retained recovery directory remains; outer payload staging
    // was cleaned without deleting the separately preserved backup.
    assert_eq!(fs::read_dir(target_dir.path()).unwrap().count(), 2);
    assert_eq!(
        fs::read_dir(backup.parent().unwrap().path())
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn staging_cleanup_failure_preserves_primary_error_or_reports_publication() {
    let target: Location = PathBuf::from("/target/report.txt").into();
    let staging: Location = PathBuf::from("/target/.staging").into();
    let published = crate::tr_format!("Item {target} was published", target = target);
    let success_error =
        staging_result(Ok(()), Err("cleanup denied".into()), &staging, &target).unwrap_err();
    assert!(success_error.starts_with(&published), "{success_error}");
    assert!(success_error.contains(&staging.display().to_string()));
    assert!(success_error.contains("cleanup denied"));

    let failure_error = staging_result(
        Err("publication denied".into()),
        Err("cleanup denied".into()),
        &staging,
        &target,
    )
    .unwrap_err();
    assert!(failure_error.starts_with("publication denied"));
    assert!(failure_error.contains("cleanup denied"));
    assert!(failure_error.contains(&staging.display().to_string()));
    assert!(!failure_error.contains(&published));
    assert_eq!(
        staging_result(Err("publication denied".into()), Ok(()), &staging, &target),
        Err("publication denied".into())
    );
}
