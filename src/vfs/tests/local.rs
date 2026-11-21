use super::*;

#[test]
fn local_mutations_keep_typed_conflicts_and_preserve_existing_items() {
    use crate::file_error::{Failure, LocalPathAction, MutationError};

    let fixture = tempfile::tempdir().unwrap();
    let root: Location = fixture.path().to_path_buf().into();
    let directory = create_directory(&root, "existing").unwrap();
    let duplicate = create_directory(&root, "existing").unwrap_err();
    let TransferError::Failed(Failure::Mutation(duplicate)) = duplicate else {
        panic!("expected a typed folder creation failure: {duplicate:?}");
    };
    assert!(matches!(
        duplicate.as_ref(),
        MutationError::LocalIo { action: LocalPathAction::CreateDirectory, path, source }
            if path == directory.path() && source.kind() == std::io::ErrorKind::AlreadyExists
    ));

    let source = directory.join("source.txt");
    let target = directory.join("target.txt");
    fs::write(source.path(), b"source").unwrap();
    fs::write(target.path(), b"target").unwrap();
    let failure =
        crate::vfs::rename_with_undo(&source, "target.txt", &mut Progress::default()).unwrap_err();
    let TransferError::Failed(Failure::Mutation(failure)) = failure else {
        panic!("expected a typed rename conflict: {failure:?}");
    };
    assert!(matches!(
        failure.as_ref(),
        MutationError::DestinationExists(path) if path == target.path()
    ));
    assert_eq!(fs::read(source.path()).unwrap(), b"source");
    assert_eq!(fs::read(target.path()).unwrap(), b"target");
}

#[test]
fn missing_directory_retains_its_path_and_original_io_error() {
    use crate::file_error::{Failure, ListingErrorKind};

    let root = tempfile::tempdir().unwrap();
    let missing = Location::Local(root.path().join("missing folder"));
    let error = read_directory(&missing, &|| false).unwrap_err();
    let TransferError::Failed(Failure::Listing(details)) = &error else {
        panic!("expected a listing failure: {error:?}");
    };
    assert_eq!(details.location, missing);
    assert!(matches!(
        &details.kind,
        ListingErrorKind::OpenLocal(source) if source.kind() == std::io::ErrorKind::NotFound
    ));
    assert!(crate::i18n::file_error(&error).contains(missing.path().to_str().unwrap()));
}

#[test]
fn local_operations_keep_existing_conflict_and_trash_policy() {
    let root = tempfile::tempdir().unwrap();
    let local = Location::Local(root.path().into());
    let a = create_directory(&local, "A").unwrap();
    let data = "data".repeat((1 << 18) + 1);
    fs::write(a.path().join("file.txt"), &data).unwrap();
    assert!(create_directory(&local, "A").is_err());
    assert!(create_directory(&local, "../escape").is_err());
    #[cfg(windows)]
    assert!(validate_name("C:").is_err());
    let b = create_directory(&local, "B").unwrap();
    let (mut progress, mut view) = Progress::tracked();
    assert_eq!(
        transfer_with_conflicts(
            std::slice::from_ref(&a),
            &b,
            TransferKind::Copy,
            &mut progress,
            |_| panic!("unexpected conflict"),
            |_, _| {},
        )
        .unwrap()
        .completed,
        1
    );
    let snapshot = view.snapshot();
    assert_eq!(
        snapshot.item.as_deref().unwrap().file_name().unwrap(),
        "file.txt"
    );
    assert_eq!(snapshot.total_written, data.len() as u64);
    assert_eq!(
        fs::read_to_string(b.path().join("A/file.txt")).unwrap(),
        data
    );
    assert!(transfer(std::slice::from_ref(&a), &b, TransferKind::Copy).is_err());
    let remote = resolve("sftp://alice@example.com/a", &local).unwrap();
    assert!(trash_entries(&[remote.with_path("/".into())]).is_err());
}

#[test]
fn failed_transfer_preflight_reports_the_source_and_leaves_the_batch_unstarted() {
    let fixture = tempfile::tempdir().unwrap();
    let root = Location::Local(fixture.path().into());
    let first = root.join("first.txt");
    let missing = root.join("missing.txt");
    fs::write(first.path(), b"keep this source").unwrap();
    let destination = create_directory(&root, "destination").unwrap();
    let (mut progress, mut view) = Progress::tracked();

    let result = transfer_with_conflicts(
        &[first.clone(), missing.clone()],
        &destination,
        TransferKind::Move,
        &mut progress,
        |_| panic!("unexpected conflict"),
        |_, _| panic!("preflight must finish before moving files"),
    );

    assert!(result.is_err());
    assert_eq!(view.snapshot().item.as_deref(), Some(missing.path()));
    assert_eq!(fs::read(first.path()).unwrap(), b"keep this source");
    assert!(fs::read_dir(destination.path()).unwrap().next().is_none());
}
