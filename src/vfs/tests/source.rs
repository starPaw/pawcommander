use super::*;

#[test]
fn remote_source_comparison_requires_observable_metadata_and_ignores_atime() {
    let expected = FileStat {
        size: Some(8),
        uid: Some(1000),
        gid: Some(1000),
        perm: Some(0o100644),
        atime: Some(10),
        mtime: Some(20),
    };
    let mut actual = expected.clone();
    actual.atime = Some(999);
    assert!(remote_source_matches(&expected, &actual));
    for change in [
        "size",
        "mtime",
        "type",
        "permissions",
        "uid",
        "gid",
        "missing-size",
        "missing-mtime",
        "missing-type",
        "missing-type-bits",
    ] {
        let mut actual = expected.clone();
        match change {
            "size" => actual.size = Some(9),
            "mtime" => actual.mtime = Some(21),
            "type" => actual.perm = Some(0o120644),
            "permissions" => actual.perm = Some(0o100600),
            "uid" => actual.uid = Some(1001),
            "gid" => actual.gid = Some(1001),
            "missing-size" => actual.size = None,
            "missing-mtime" => actual.mtime = None,
            "missing-type" => actual.perm = None,
            "missing-type-bits" => actual.perm = Some(0o644),
            _ => unreachable!(),
        }
        assert!(!remote_source_matches(&expected, &actual), "{change}");
        if change.starts_with("missing") {
            assert!(!remote_source_matches(&actual, &actual), "{change}");
        }
    }
    // Identical observable metadata cannot distinguish a same-second edit
    // or an inode replacement on SFTP v3. Do not claim snapshot semantics.
    assert!(remote_source_matches(&expected, &expected));
}

#[test]
fn remote_source_recheck_identifies_missing_fields_at_the_full_path() {
    use super::super::transfer::{EntryMetadata, TransferSourceEntry};
    use crate::file_error::{Failure, ScanError};
    use std::mem::discriminant;

    let source = resolve(
        "sftp://user@localhost/folder/report",
        &Location::Local(PathBuf::from("/")),
    )
    .unwrap();
    let stat = FileStat {
        size: Some(8),
        uid: None,
        gid: None,
        perm: Some(0o100644),
        atime: None,
        mtime: Some(20),
    };
    let entry = TransferSourceEntry {
        metadata: EntryMetadata::Remote(stat.clone()),
        children: None,
    };
    for (perm, size, mtime, expected) in [
        (
            None,
            stat.size,
            stat.mtime,
            ScanError::MissingSftpFileType(source.clone()),
        ),
        (
            Some(0o644),
            stat.size,
            stat.mtime,
            ScanError::MissingSftpFileType(source.clone()),
        ),
        (
            stat.perm,
            None,
            stat.mtime,
            ScanError::MissingSftpFileSize(source.clone()),
        ),
        (
            stat.perm,
            stat.size,
            None,
            ScanError::MissingSftpModificationTime(source.clone()),
        ),
        (
            stat.perm,
            None,
            None,
            ScanError::MissingSftpFileVersion(source.clone()),
        ),
    ] {
        let actual = EntryMetadata::Remote(FileStat {
            perm,
            size,
            mtime,
            ..stat.clone()
        });
        let error = entry.verify_metadata(&source, &actual).unwrap_err();
        let TransferError::Failed(Failure::Scan(reason)) = error else {
            panic!("unexpected source check failure: {error:?}");
        };
        assert_eq!(discriminant(reason.as_ref()), discriminant(&expected));
        match *reason {
            ScanError::MissingSftpFileType(location)
            | ScanError::MissingSftpFileSize(location)
            | ScanError::MissingSftpModificationTime(location)
            | ScanError::MissingSftpFileVersion(location) => assert_eq!(location, source),
            other => panic!("unexpected scan error: {other:?}"),
        }
    }
    let directory = EntryMetadata::Remote(FileStat {
        perm: Some(0o40755),
        size: None,
        mtime: None,
        ..stat
    });
    assert!(directory.check_sftp_source(&source).is_ok());
}

#[test]
fn transfer_source_final_check_catches_early_edits_and_preserves_target() {
    let root = tempfile::tempdir().unwrap();
    let source = Location::Local(root.path().join("source"));
    let target = Location::Local(root.path().join("target"));
    fs::create_dir(source.path()).unwrap();
    fs::write(source.join("a-early").path(), b"early").unwrap();
    fs::write(source.join("z-late").path(), b"late").unwrap();
    fs::write(target.path(), b"previous").unwrap();
    let expected = entry_metadata(&target).unwrap().unwrap();
    let error = copy_transfer_checked(
        &TransferItem::new(source.clone(), source.clone(), target.clone()),
        Some(&expected),
        &mut Progress::default(),
        &mut |path, phase| {
            if path == &source.join("z-late") && phase == CopyPhase::EntryCopied {
                fs::write(source.join("a-early").path(), b"changed early").unwrap();
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert!(error.contains("changed during copying"), "{error}");
    assert_eq!(fs::read(target.path()).unwrap(), b"previous");
    assert_eq!(
        fs::read(source.join("a-early").path()).unwrap(),
        b"changed early"
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
}

#[cfg(unix)]
#[test]
fn sftp_name_validation_rejects_non_utf8_without_lossy_conversion() {
    use std::os::unix::ffi::OsStringExt;
    let source = Location::Local(PathBuf::from("source"));
    let raw = OsString::from_vec(b"bad-\xff-name".to_vec());
    assert!(validate_source_child_name(&source, &raw)
        .unwrap_err()
        .contains("UTF-8"));
    // A literal replacement character is valid Unicode, but must never
    // substitute for the invalid byte and select a different source file.
    assert!(validate_source_child_name(&source, OsStr::new("bad-�-name")).is_ok());
}

#[cfg(unix)]
#[test]
fn sftp_manifest_rejects_unsupported_nested_names_before_staging() {
    for name in unsupported_sftp_names() {
        let root = tempfile::tempdir().unwrap();
        let source = Location::Local(root.path().join("source"));
        let target = Location::Local(root.path().join("target"));
        fs::create_dir_all(source.join("nested").path()).unwrap();
        let unsupported = source.join("nested").join(&name);
        fs::write(unsupported.path(), b"source bytes").unwrap();
        fs::write(target.path(), b"previous target").unwrap();
        let expected = entry_metadata(&target).unwrap().unwrap();
        let error = copy_transfer_checked(
            &TransferItem::new(source.clone(), source.clone(), target.clone()),
            Some(&expected),
            &mut Progress::default(),
            &mut |_, _| panic!("unsupported names must fail before copying"),
        )
        .unwrap_err();
        assert!(error.contains("SFTP"), "{error}");
        assert!(!error.contains("Осталась"), "{error}");
        assert_eq!(fs::read(target.path()).unwrap(), b"previous target");
        assert_eq!(fs::read(unsupported.path()).unwrap(), b"source bytes");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);

        // The stricter contract applies only to SFTP transfers. A native
        // local copy still preserves names supported by the local volume.
        let local_target =
            create_directory(&root.path().to_path_buf().into(), "local-copy").unwrap();
        transfer(
            std::slice::from_ref(&source),
            &local_target,
            TransferKind::Copy,
        )
        .unwrap();
        assert_eq!(
            fs::read(local_target.join("source/nested").join(&name).path()).unwrap(),
            b"source bytes"
        );
    }
}
