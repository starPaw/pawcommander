use super::*;
use crate::file_error::{AccessAction, AccessCause, Failure, MutationError};
use std::{fs, os::unix::fs::symlink};

fn document_cause(
    error: TransferError,
    action: CollectionAction,
    file: &Location,
) -> TransferError {
    let TransferError::Failed(Failure::Collection(error)) = error else {
        panic!("expected a collection error, got {error:?}");
    };
    let CollectionError::Document {
        action: actual,
        file: document,
        cause,
    } = *error
    else {
        panic!("expected the portable document context, got {error:?}");
    };
    assert_eq!(actual, action);
    assert_eq!(&document, file);
    *cause
}

#[test]
fn references_follow_external_renames_and_reject_replaced_or_deleted_items() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let original = temp.path().join("original");
    fs::create_dir(&original).unwrap();
    let file = original.join("report.txt");
    fs::write(&file, b"original").unwrap();
    let alias = original.join("hard-link.txt");
    fs::hard_link(&file, &alias).unwrap();
    let mut catalog = Catalog::default();
    let index = catalog.create("Work").unwrap();
    let group = &mut catalog.collections[index];
    let alias_location = Location::Local(alias.clone());
    group
        .add(
            &[
                Location::Local(file.clone()),
                Location::Local(original.clone()),
                alias_location,
            ],
            &parse_tags("Open, open, Отчёт").unwrap(),
            MAX_MEMBERS,
            &mut Progress::default(),
        )
        .unwrap();
    assert_eq!(
        group.members.len(),
        2,
        "a hard link shares the referenced file"
    );

    let profile = ron::to_string(&catalog).unwrap();
    let mut loaded: Catalog = ron::from_str(&profile).unwrap();
    let group = &mut loaded.collections[0];
    fs::remove_file(&alias).unwrap();
    let moved = temp.path().join("renamed");
    fs::rename(&original, &moved).unwrap();
    let old_moved_file = moved.join("report.txt");
    let moved_file = moved.join("renamed.txt");
    fs::rename(old_moved_file, &moved_file).unwrap();
    fs::create_dir(&original).unwrap();
    fs::write(&file, b"replacement at old name").unwrap();
    assert!(matches!(
        group.refresh(&mut Progress::default()).unwrap().as_slice(),
        [None, None]
    ));
    assert_eq!(group.members[0].last_path().unwrap(), moved_file);
    assert_eq!(group.members[1].last_path().unwrap(), moved);
    assert_eq!(group.members[0].tags, ["Open", "Отчёт"]);
    fs::remove_file(&moved_file).unwrap();
    fs::write(&moved_file, b"different item at current name").unwrap();
    assert!(matches!(
        group.refresh(&mut Progress::default()).unwrap().as_slice(),
        [Some(Unavailable::Missing), None]
    ));
    assert_eq!(group.members[0].last_path().unwrap(), moved_file);
    assert_eq!(fs::read(&file).unwrap(), b"replacement at old name");

    group.relink(0, &Location::Local(file.clone())).unwrap();
    assert_eq!(group.members[0].resolve().unwrap(), file);
    assert_eq!(group.members[0].tags, ["Open", "Отчёт"]);
}

#[test]
fn portable_collection_round_trips_below_a_different_root_without_overwriting() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let old_root = temp.path().join("source");
    let new_root = temp.path().join("destination");
    for root in [&old_root, &new_root] {
        fs::create_dir_all(root.join("reports")).unwrap();
        fs::write(root.join("reports/Итог.txt"), b"report bytes").unwrap();
    }
    let mut catalog = Catalog::default();
    let index = catalog.create("Разбор").unwrap();
    let group = &mut catalog.collections[index];
    let mut progress = Progress::default();
    group
        .add(
            &[
                Location::Local(old_root.join("reports/Итог.txt")),
                Location::Local(old_root.join("reports")),
            ],
            &parse_tags("готово, Work").unwrap(),
            MAX_MEMBERS,
            &mut progress,
        )
        .unwrap();

    let old = Location::Local(old_root.clone());
    let new = Location::Local(new_root.clone());
    let name = "reports.pawcollection.json";
    let output = export(&old, name, group, &mut progress).unwrap();
    let contents = fs::read_to_string(output.path()).unwrap();
    assert!(!contents.contains(old_root.to_str().unwrap()));
    assert!(!contents.contains("bookmark"));
    assert!(contents.contains("reports/Итог.txt"));
    let collision = export(&old, name, group, &mut progress).unwrap_err();
    assert!(
        matches!(document_cause(collision, CollectionAction::Export, &output),
        TransferError::Failed(Failure::Mutation(problem))
            if matches!(problem.as_ref(), MutationError::DestinationExists(path) if path == &output))
    );
    assert_eq!(fs::read_to_string(output.path()).unwrap(), contents);
    fs::copy(output.path(), new_root.join(name)).unwrap();
    let imported = import(&new, &new.join(name), &mut progress).unwrap();
    let index = catalog.insert_imported(imported).unwrap();
    let group = &mut catalog.collections[index];
    assert_eq!(group.name, "Разбор (2)");
    assert_eq!(
        group.members[0].resolve().unwrap(),
        new_root.join("reports/Итог.txt")
    );
    assert_eq!(group.members[0].tags, ["готово", "Work"]);
    assert_eq!(
        group.members[1].resolve().unwrap(),
        new_root.join("reports")
    );
    assert!(group.members[1].is_folder());
    assert_eq!(
        fs::read(new_root.join("reports/Итог.txt")).unwrap(),
        b"report bytes"
    );
    assert!(export(&old, "outside.pawcollection.json", group, &mut progress).is_err());
    assert!(!old_root.join("outside.pawcollection.json").exists());
}

#[test]
fn import_rejects_escapes_and_incomplete_copies_before_creating_a_collection() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temp.path().join("root");
    let elsewhere = temp.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&elsewhere).unwrap();
    let outside = elsewhere.join("outside.txt");
    fs::write(&outside, b"outside").unwrap();
    fs::write(root.join("good.txt"), b"inside").unwrap();
    symlink(&elsewhere, root.join("link")).unwrap();
    let base = Location::Local(root.clone());
    let manifest = base.join("import.pawcollection.json");
    for (case, suspect) in [
        PathBuf::from("../outside/outside.txt"),
        outside.clone(),
        PathBuf::from("link/outside.txt"),
        PathBuf::from("missing.txt"),
    ]
    .into_iter()
    .enumerate()
    {
        let item = |path: &Path| PortableMember {
            path: path.into(),
            folder: false,
            tags: Vec::new(),
        };
        let document = Portable {
            version: PORTABLE_VERSION,
            name: "Fixture".into(),
            members: vec![item(Path::new("good.txt")), item(&suspect)],
        };
        fs::write(manifest.path(), serde_json::to_vec(&document).unwrap()).unwrap();
        let cause = document_cause(
            import(&base, &manifest, &mut Progress::default())
                .err()
                .unwrap(),
            CollectionAction::Import,
            &manifest,
        );
        let TransferError::Failed(Failure::Collection(entry)) = cause else {
            panic!("missing entry context for {suspect:?}");
        };
        let CollectionError::Entry {
            row: 2,
            path: Some(path),
            cause,
        } = *entry
        else {
            panic!("expected the failing entry for {suspect:?}");
        };
        match case {
            0 | 1 => {
                assert_eq!(path, suspect);
                assert!(
                    matches!(*cause, TransferError::Failed(Failure::Collection(problem))
                    if matches!(*problem, CollectionError::InvalidPath))
                );
            }
            2 => {
                assert_eq!(path, root.join(suspect));
                assert!(
                    matches!(*cause, TransferError::Failed(Failure::Collection(problem))
                    if matches!(problem.as_ref(), CollectionError::OutsideFolder { resolved, folder }
                        if resolved == &outside && folder == &root))
                );
            }
            3 => {
                assert_eq!(path, root.join(suspect));
                assert!(
                    matches!(*cause, TransferError::Failed(Failure::Access(problem))
                    if problem.location.path() == path && problem.action == AccessAction::ReadMetadata
                        && matches!(&problem.cause, AccessCause::Local(io)
                            if io.raw_os_error() == Some(libc::ENOENT)))
                );
            }
            _ => unreachable!(),
        }
    }
    assert_eq!(fs::read(root.join("good.txt")).unwrap(), b"inside");
    assert_eq!(fs::read(outside).unwrap(), b"outside");
}

#[test]
fn portable_document_errors_retain_the_selected_path_and_original_cause() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = Location::Local(temp.path().to_owned());
    let file = root.join("import.pawcollection.json");
    let mut progress = Progress::default();
    let read = |progress: &mut Progress| {
        document_cause(
            import(&root, &file, progress).err().unwrap(),
            CollectionAction::Import,
            &file,
        )
    };
    assert!(
        matches!(read(&mut progress), TransferError::Failed(Failure::Access(problem))
        if problem.location == file && matches!(&problem.cause, AccessCause::Local(io)
            if io.raw_os_error() == Some(libc::ENOENT)))
    );

    let damaged = b"{\n  \"version\": ";
    fs::write(file.path(), damaged).unwrap();
    assert!(
        matches!(read(&mut progress), TransferError::Failed(Failure::Collection(problem))
        if matches!(problem.as_ref(), CollectionError::Json(cause) if cause.is_eof() && cause.line() == 2))
    );
    assert_eq!(fs::read(file.path()).unwrap(), damaged);

    fs::write(
        file.path(),
        br#"{"version":19,"name":"Future","members":[]}"#,
    )
    .unwrap();
    assert!(
        matches!(read(&mut progress), TransferError::Failed(Failure::Collection(problem))
        if matches!(*problem, CollectionError::Version(19)))
    );

    fs::File::create(file.path())
        .unwrap()
        .set_len(MAX_PORTABLE_BYTES as u64 + 1)
        .unwrap();
    assert!(
        matches!(read(&mut progress), TransferError::Failed(Failure::Collection(problem))
        if matches!(*problem, CollectionError::ByteLimit(MAX_PORTABLE_BYTES)))
    );
}

#[test]
fn export_reports_a_missing_entry_without_publishing_and_preserves_stop() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = Location::Local(temp.path().to_owned());
    let input = root.join("gone.txt");
    fs::write(input.path(), b"fixture").unwrap();
    let mut group = Collection {
        name: "Fixture".into(),
        members: vec![Member::capture(input.path(), Vec::new()).unwrap()],
    };
    fs::remove_file(input.path()).unwrap();
    let name = "output.pawcollection.json";
    let target = root.join(name);
    let mut progress = Progress::default();
    let cause = document_cause(
        export(&root, name, &mut group, &mut progress).unwrap_err(),
        CollectionAction::Export,
        &target,
    );
    let TransferError::Failed(Failure::Collection(entry)) = cause else {
        panic!("expected an entry error");
    };
    let CollectionError::Entry {
        row: 1,
        path: Some(path),
        cause,
    } = *entry
    else {
        panic!("expected the original member at entry 1");
    };
    assert_eq!(path, input.path());
    assert!(
        matches!(*cause, TransferError::Failed(Failure::Collection(problem))
        if matches!(problem.as_ref(), CollectionError::Unavailable(Unavailable::Access(access))
            if access.location == input && matches!(&access.cause, AccessCause::Local(io)
                if io.raw_os_error() == Some(libc::ENOENT))))
    );
    assert!(!target.path().exists());
    assert_eq!(group.members[0].last_path().as_deref(), Some(input.path()));
    progress.cancellation().request();
    assert!(matches!(
        export(&root, name, &mut group, &mut progress),
        Err(TransferError::Cancelled)
    ));
    assert!(temp.path().read_dir().unwrap().next().is_none());
}
