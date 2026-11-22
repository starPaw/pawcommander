use super::*;

#[cfg(target_os = "macos")]
#[test]
fn smb_file_staging_cleans_through_the_open_parent() {
    use super::super::transfer::{with_smb_file_staging, FILE_STAGING_PAYLOAD};
    use rustix::fs::{open, Mode, OFlags};

    let fixture = tempfile::tempdir().unwrap();
    let parent = fixture.path().join("parent");
    let held = fixture.path().join("held");
    fs::create_dir(&parent).unwrap();
    let target = Location::Local(parent.join("new.txt"));
    let handle = open(&parent, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
    let cancel = Cancellation::default();

    let result = with_smb_file_staging(&target, &cancel, &handle, |stage, _| {
        fs::write(stage.path().join(FILE_STAGING_PAYLOAD), b"ours").unwrap();
        fs::rename(&parent, &held).unwrap();
        fs::create_dir_all(stage.path()).unwrap();
        fs::write(stage.path().join(FILE_STAGING_PAYLOAD), b"preserve").unwrap();
        Err(TransferError::Cancelled)
    });

    assert!(matches!(result, Err(TransferError::Cancelled)));
    assert_eq!(fs::read_dir(held).unwrap().count(), 0);
    let replacement = fs::read_dir(parent)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        fs::read(replacement.join(FILE_STAGING_PAYLOAD)).unwrap(),
        b"preserve"
    );
}

#[test]
fn staging_cleanup_preserves_a_replacement_directory() {
    use super::super::transfer::with_staging_typed;
    use crate::file_error::{Failure, StagingError};

    let fixture = tempfile::tempdir().unwrap();
    let target: Location = fixture.path().join("target").into();
    let original = fixture.path().join("original-staging");
    let mut replacement = None;
    let error = with_staging_typed(&target, None, |staging, _| {
        fs::rename(staging.path(), &original).unwrap();
        fs::create_dir(staging.path()).unwrap();
        fs::write(staging.path().join("foreign"), b"preserve").unwrap();
        replacement = Some(staging.path().to_path_buf());
        Err("injected failure".into())
    })
    .unwrap_err();
    let TransferError::Failed(Failure::StagingCleanup(error)) = error else {
        panic!("expected both operation and staging failures");
    };
    assert!(
        matches!(&error.operation, Some(TransferError::Failed(Failure::Message(message)))
        if message == "injected failure")
    );
    assert!(
        matches!(&error.cleanup, TransferError::Failed(Failure::Staging(reason))
        if matches!(reason.as_ref(), StagingError::IdentityChanged(path) if path == &error.staging))
    );
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
    use crate::file_error::Failure;

    let staging: Location = PathBuf::from("/private-staging").into();
    let target: Location = PathBuf::from("/destination").into();
    let result = StagingCleanupError::combine(
        Err(TransferError::PublicationUnconfirmed(Failure::Message(
            "reply lost".into(),
        ))),
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into()),
        &staging,
        &target,
    );
    match result {
        Err(TransferError::PublicationUnconfirmed(Failure::StagingCleanup(error))) => {
            assert_eq!(error.staging, staging);
            assert_eq!(error.target, target);
            assert!(matches!(
                error.operation,
                Some(TransferError::PublicationUnconfirmed(Failure::Message(ref message)))
                    if message == "reply lost"
            ));
            assert!(matches!(
                error.cleanup,
                TransferError::Failed(Failure::Io { ref source, .. })
                    if source.kind() == std::io::ErrorKind::PermissionDenied
            ));
        }
        other => panic!("uncertain publication became retryable: {other:?}"),
    }
}

#[test]
fn failed_initial_backup_rename_retains_original_destination_and_cleans_staging() {
    use super::super::transfer::with_staging_typed;
    use crate::file_error::{Failure, PublicationError};

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

    let error = fs_ops::copy_atomically_cancellable(
        source.path(),
        target.path(),
        &mut Progress::default(),
        |payload, _| {
            assert_eq!(fs::read(payload).unwrap(), b"new source payload");
            with_staging_typed(&target, None, |staging, preserve| {
                publish_replacement_with_backup_rename(
                    &payload.to_path_buf().into(),
                    &target,
                    staging,
                    &expected,
                    preserve,
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
        &mut |_| Ok(()),
    )
    .unwrap_err();

    assert_eq!(rename_calls, 1);
    let backup_path = backup_path.unwrap();
    assert!(!backup_path.path().exists());
    assert!(!backup_path.parent().unwrap().path().exists());
    assert_eq!(fs::read(source.path()).unwrap(), b"new source payload");
    assert_eq!(fs::read(target.path()).unwrap(), b"previous destination");
    assert_eq!(fs::read_dir(target_dir.path()).unwrap().count(), 1);
    assert!(matches!(
        &error,
        TransferError::Failed(Failure::Publication(reason))
            if matches!(reason.as_ref(), PublicationError::Backup {
                target: location, backup, cause: TransferError::Failed(Failure::Message(cause)),
            } if location == &target && backup == &backup_path
                && cause == "injected first backup rename denial")
    ));
    let error = crate::i18n::file_error(&error);
    assert!(error.starts_with("injected first backup rename denial"));
    assert!(error.contains(&target.display().to_string()), "{error}");
    assert!(
        !error.contains(&backup_path.display().to_string()),
        "guidance must not direct the user to a removed backup: {error}"
    );
}

#[test]
fn replacement_rollback_preserves_backup_when_target_is_recreated() {
    use super::super::transfer::with_staging_typed;
    use crate::file_error::{Failure, MutationError, PublicationError};

    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let source = root.join("report.txt");
    let target_dir = create_directory(&root, "target").unwrap();
    let target = target_dir.join("report.txt");
    fs::write(source.path(), b"new source payload").unwrap();
    fs::write(target.path(), b"previous destination").unwrap();
    let expected = entry_metadata(&target).unwrap().unwrap();
    let mut backup_path = None;

    let error = fs_ops::copy_atomically_cancellable(
        source.path(),
        target.path(),
        &mut Progress::default(),
        |payload, _| {
            with_staging_typed(&target, None, |staging, preserve| {
                let result = publish_replacement_with_backup_rename(
                    &payload.to_path_buf().into(),
                    &target,
                    staging,
                    &expected,
                    preserve,
                    |from, to| {
                        rename_for_publication(from, to)?;
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
        &mut |_| Ok(()),
    )
    .unwrap_err();

    let backup = backup_path.unwrap();
    assert_eq!(fs::read(source.path()).unwrap(), b"new source payload");
    assert_eq!(fs::read(target.path()).unwrap(), b"external writer");
    assert_eq!(fs::read(backup.path()).unwrap(), b"previous destination");
    assert!(backup.parent().unwrap().path().is_dir());
    let TransferError::Failed(Failure::Publication(reason)) = &error else {
        panic!("expected a typed rollback error: {error:?}");
    };
    let PublicationError::Rollback {
        target: location,
        backup: saved,
        cause,
        restore,
    } = reason.as_ref()
    else {
        panic!("expected both rename causes: {reason:?}");
    };
    assert_eq!(location, &target);
    assert_eq!(saved, &backup);
    for (failure, source_path) in [(cause, "payload"), (restore, "previous")] {
        assert!(matches!(
            failure,
            TransferError::Failed(Failure::Mutation(reason))
                if matches!(reason.as_ref(), MutationError::RenameLocal { from, to, source }
                    if from.file_name().is_some_and(|name| name == source_path)
                        && to == target.path() && source.kind() == std::io::ErrorKind::AlreadyExists)
        ));
    }
    let error = crate::i18n::file_error(&error);
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
