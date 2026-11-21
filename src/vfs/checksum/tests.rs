use super::*;
use crate::file_error::{Failure, ManifestLineError};
use crate::operations::Cancellation;
use std::fs;

const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

pub(crate) fn checksum_round_trip(directory: &Location) {
    let unchanged = directory.join("file with spaces.txt");
    let edited = directory.join("edited.txt");
    let absent = directory.join("absent.txt");
    let paths = [unchanged.clone(), edited.clone(), absent.clone()];
    for path in &paths {
        fs::write(path.path(), b"abc").unwrap();
    }
    let target = directory.join("SHA256SUMS");
    let created =
        create_checksum_manifest(directory, "SHA256SUMS", &paths, &mut Progress::default())
            .unwrap();
    assert_eq!(created, target);
    let manifest = fs::read(target.path()).unwrap();
    assert!(manifest.starts_with(format!("{ABC}  file with spaces.txt\n").as_bytes()));
    let conflict =
        create_checksum_manifest(directory, "SHA256SUMS", &paths, &mut Progress::default())
            .unwrap_err();
    let canonical_target = target.with_path(target.path().canonicalize().unwrap());
    assert!(matches!(
        conflict,
        TransferError::Failed(Failure::Mutation(reason))
            if matches!(reason.as_ref(), MutationError::DestinationExists(location) if location == &canonical_target)
    ));
    assert_eq!(fs::read(target.path()).unwrap(), manifest);

    let mut before = Progress::default();
    verify_checksum_manifest(&target, &mut before).unwrap();
    assert!(before
        .results
        .items
        .iter()
        .all(|row| row.outcome == Outcome::Completed));

    fs::write(edited.path(), b"xyz").unwrap();
    fs::remove_file(absent.path()).unwrap();
    let mut after = Progress::default();
    let report = verify_checksum_manifest(&target, &mut after).unwrap();
    assert_eq!(
        report,
        ChecksumVerification {
            matched: 1,
            changed: 1,
            missing: 1,
            errors: 0,
            cancelled: false,
        }
    );
    for (row, path, reason) in [
        (
            &after.results.items[0],
            &unchanged,
            crate::i18n::text!("SHA-256 совпала"),
        ),
        (
            &after.results.items[1],
            &edited,
            crate::i18n::text!("Файл изменён (SHA-256 не совпала)"),
        ),
        (
            &after.results.items[2],
            &absent,
            crate::i18n::text!("Файл отсутствует"),
        ),
    ] {
        assert_eq!(row.source, path.display());
        assert_eq!(row.reason.as_deref(), Some(reason));
    }
    assert_eq!(fs::read(unchanged.path()).unwrap(), b"abc");
    assert_eq!(fs::read(edited.path()).unwrap(), b"xyz");
    assert_eq!(fs::read(target.path()).unwrap(), manifest);
    #[cfg(target_os = "macos")]
    if target.remote() {
        check_sftp_read_denial(directory, &target, &unchanged);
    }
}

#[cfg(target_os = "macos")]
fn check_sftp_read_denial(directory: &Location, manifest: &Location, source: &Location) {
    use crate::file_error::{AccessAction, AccessCause};
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(source.path()).unwrap().permissions();
    fs::set_permissions(source.path(), fs::Permissions::from_mode(0o000)).unwrap();
    let mut progress = Progress::default();
    let verification = verify_checksum_manifest(manifest, &mut progress);
    let creation = create_checksum_manifest(
        directory,
        "blocked.sha256",
        std::slice::from_ref(source),
        &mut Progress::default(),
    );
    fs::set_permissions(source.path(), mode).unwrap();

    assert_eq!(
        verification.unwrap(),
        ChecksumVerification {
            matched: 0,
            changed: 1,
            missing: 1,
            errors: 1,
            cancelled: false,
        }
    );
    let row = &progress.results.items[0];
    assert_eq!(row.source, source.display());
    assert_eq!(row.outcome, Outcome::Failed);
    let location = source.with_path(source.path().canonicalize().unwrap());
    let reason = row.reason.as_deref().unwrap();
    assert!(reason.contains(&location.display()) && reason.contains("SFTP(3)"));
    assert!(matches!(
        creation,
        Err(TransferError::Failed(Failure::Access(error)))
            if error.location == location && error.action == AccessAction::OpenFile
                && matches!(&error.cause, AccessCause::Sftp(source)
                    if source.code() == ssh2::ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_PERMISSION_DENIED))
    ));
    assert!(!directory.join("blocked.sha256").path().exists());
    assert_eq!(fs::read(source.path()).unwrap(), b"abc");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 3);
}

#[test]
fn checksum_manifest_distinguishes_matches_changes_and_missing_files() {
    let root = tempfile::tempdir().unwrap();
    checksum_round_trip(&Location::Local(root.path().to_owned()));
}

#[test]
fn checksum_parser_validates_paths_and_normalized_duplicates_before_reading_files() {
    let cancel = Cancellation::default();
    let location = Location::Local("SHA256SUMS".into());
    let tagged = format!(
        "\u{feff}# SHA-256\r\nSHA256 (./nested/data (copy)) = {}\r\n",
        ABC.to_uppercase()
    );
    let entries = manifest::parse(tagged.as_bytes(), &location, &cancel).unwrap();
    assert_eq!(entries[0].path, Path::new("nested/data (copy)"));
    assert_eq!(entries[0].digest[0..4], [0xba, 0x78, 0x16, 0xbf]);
    for (invalid, expected) in [
        (format!("{ABC}  ../outside.txt\n"), ManifestLineError::Path),
        (format!("{ABC}  /outside.txt\n"), ManifestLineError::Path),
        (
            format!("{ABC}  sub/../outside.txt\n"),
            ManifestLineError::Path,
        ),
        (format!("{ABC}  dir/\n"), ManifestLineError::Path),
        (format!("{ABC}  .\n"), ManifestLineError::Path),
        (
            format!("\\{ABC}  escaped\\qname\n"),
            ManifestLineError::Escape,
        ),
        (
            format!("{}x  invalid-digest\n", &ABC[..63]),
            ManifestLineError::Digest,
        ),
        (format!("{ABC}  nul\0name\n"), ManifestLineError::Path),
        (
            format!("{ABC}:missing separator\n"),
            ManifestLineError::Format,
        ),
    ] {
        let bytes = format!("# A comment\r\n\n{invalid}");
        let error = manifest::parse(bytes.as_bytes(), &location, &cancel).unwrap_err();
        assert!(
            matches!(
                error,
                TransferError::Failed(Failure::Checksum(error))
                    if matches!(error.as_ref(), ChecksumError::InvalidLine { manifest, row: 3, source }
                        if manifest == &location && *source == expected)
            ),
            "{invalid:?}"
        );
    }
    let bytes = format!("{ABC}  nested/a\n{ABC} *./nested/a\n");
    assert!(matches!(
        manifest::parse(bytes.as_bytes(), &location, &cancel),
        Err(TransferError::Failed(Failure::Checksum(error)))
            if matches!(error.as_ref(), ChecksumError::DuplicatePath { manifest, row: 2, path }
                if manifest == &location && path == Path::new("nested/a"))
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn checksum_manifest_interoperates_with_macos_shasum() {
    use std::process::Command;

    let root = tempfile::tempdir().unwrap();
    let directory = Location::Local(root.path().to_owned());
    let file_name = "размер \\ file\nname.txt";
    let file = directory.join(file_name);
    fs::write(file.path(), b"abc").unwrap();
    create_checksum_manifest(&directory, "SHA256SUMS", &[file], &mut Progress::default()).unwrap();
    let output = Command::new("/usr/bin/shasum")
        .current_dir(root.path())
        .args(["-a", "256", "-c", "SHA256SUMS"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let output = Command::new("/usr/bin/shasum")
        .current_dir(root.path())
        .args(["-a", "256", "--tag", "--", file_name])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let imported = directory.join("external.sha256");
    fs::write(imported.path(), output.stdout).unwrap();
    verify_checksum_manifest(&imported, &mut Progress::default()).unwrap();

    let file = directory.join("carriage-return\r");
    fs::write(file.path(), b"abc").unwrap();
    let destination = "escaped.sha256";
    create_checksum_manifest(&directory, destination, &[file], &mut Progress::default()).unwrap();
    let manifest = directory.join(destination);
    assert!(fs::read(manifest.path())
        .unwrap()
        .ends_with(b"carriage-return\\r\n"));
    verify_checksum_manifest(&manifest, &mut Progress::default()).unwrap();
}

#[cfg(unix)]
#[test]
fn checksum_manifest_refuses_links_and_accepts_real_nested_files() {
    let root = tempfile::tempdir().unwrap();
    let inside = root.path().join("inside");
    let outside = root.path().join("outside");
    fs::create_dir_all(inside.join("real")).unwrap();
    fs::create_dir(&outside).unwrap();
    for directory in [&inside.join("real"), &outside] {
        fs::write(directory.join("data"), b"abc").unwrap();
    }
    std::os::unix::fs::symlink(&outside, inside.join("link-dir")).unwrap();
    std::os::unix::fs::symlink(outside.join("data"), inside.join("link-file")).unwrap();
    let manifest = Location::Local(inside.join("SHA256SUMS"));
    fs::write(
        manifest.path(),
        format!("{ABC}  real/data\n{ABC}  link-dir/data\n{ABC}  link-file\n"),
    )
    .unwrap();
    let mut progress = Progress::default();
    let report = verify_checksum_manifest(&manifest, &mut progress).unwrap();
    assert_eq!(
        report,
        ChecksumVerification {
            matched: 1,
            changed: 0,
            missing: 0,
            errors: 2,
            cancelled: false,
        }
    );
    assert_eq!(fs::read(outside.join("data")).unwrap(), b"abc");
}

#[test]
fn checksum_manifest_stops_in_staging_and_preserves_inputs() {
    let root = tempfile::tempdir().unwrap();
    let directory = Location::Local(root.path().to_owned());
    let source = directory.join("original");
    fs::write(source.path(), b"abc").unwrap();
    let tracked = root.path().to_owned();
    let cancel = Cancellation::at_checkpoint(move || {
        fs::read_dir(&tracked).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".pawcommander-transfer-")
        })
    });
    let mut progress = Progress::with_cancellation(cancel);
    let result = create_checksum_manifest(
        &directory,
        "SHA256SUMS",
        std::slice::from_ref(&source),
        &mut progress,
    );
    assert!(matches!(result, Err(TransferError::Cancelled)));
    assert!(progress.results.cancelled);
    assert_eq!(fs::read(source.path()).unwrap(), b"abc");
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn checksum_invalid_manifest_keeps_the_diagnostic_on_the_manifest_row() {
    let temporary = tempfile::tempdir().unwrap();
    let root = Location::Local(temporary.path().canonicalize().unwrap());
    let path = root.join("bad.sha256");
    let bytes = format!("# fixture\n{ABC}  missing.txt\n{ABC} *./missing.txt\n");
    fs::write(path.path(), &bytes).unwrap();
    let mut progress = Progress::default();
    let error = verify_checksum_manifest(&path, &mut progress).unwrap_err();
    assert!(matches!(
        &error,
        TransferError::Failed(Failure::Checksum(error))
            if matches!(error.as_ref(), ChecksumError::DuplicatePath { manifest, row: 3, path: relative }
                if manifest == &path && relative == Path::new("missing.txt"))
    ));
    let [row] = progress.results.items.as_slice() else {
        panic!("the invalid manifest must have one history row");
    };
    assert_eq!(row.source, path.display());
    assert_eq!(row.outcome, Outcome::Failed);
    assert_eq!(
        row.reason.as_deref(),
        Some(crate::i18n::file_error(&error).as_str())
    );
    assert_eq!(fs::read(path.path()).unwrap(), bytes.as_bytes());
}

#[test]
fn checksum_verification_stop_keeps_completed_results() {
    let temporary = tempfile::tempdir().unwrap();
    let root = Location::Local(temporary.path().canonicalize().unwrap());
    for name in ["first.txt", "stopping.txt", "later.txt"] {
        fs::write(root.join(name).path(), b"abc").unwrap();
    }
    let manifest = root.join("SHA256SUMS");
    fs::write(
        manifest.path(),
        format!("{ABC}  first.txt\n{ABC}  stopping.txt\n{ABC}  later.txt\n"),
    )
    .unwrap();
    let (mut progress, mut view) = Progress::tracked();
    let cancel = progress.cancellation();
    let stopping = root.join("stopping.txt");
    cancel.set_checkpoint(move || view.snapshot().item.as_deref() == Some(stopping.path()));
    let report = verify_checksum_manifest(&manifest, &mut progress);
    // Release the view held by the checkpoint, which owns another cancellation handle.
    cancel.set_checkpoint(|| false);
    assert_eq!(
        report.unwrap(),
        ChecksumVerification {
            matched: 1,
            cancelled: true,
            ..ChecksumVerification::default()
        }
    );
    assert!(progress.results.cancelled);
    let record = std::mem::take(&mut progress.results)
        .finish("Verify fixture".into(), 0, &Ok("Stopped".into()))
        .unwrap();
    assert_eq!(
        record
            .items
            .iter()
            .map(|row| row.outcome)
            .collect::<Vec<_>>(),
        [Outcome::Completed, Outcome::Cancelled, Outcome::NotStarted,]
    );
    assert_eq!(fs::read(root.join("first.txt").path()).unwrap(), b"abc");
    assert_eq!(fs::read(root.join("stopping.txt").path()).unwrap(), b"abc");
    assert_eq!(fs::read(root.join("later.txt").path()).unwrap(), b"abc");
}
