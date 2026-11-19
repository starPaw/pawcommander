use super::*;
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
    create_checksum_manifest(directory, "SHA256SUMS", &paths, &mut Progress::default()).unwrap();
    let manifest = fs::read(target.path()).unwrap();
    assert!(manifest.starts_with(format!("{ABC}  file with spaces.txt\n").as_bytes()));
    assert!(
        create_checksum_manifest(directory, "SHA256SUMS", &paths, &mut Progress::default())
            .is_err()
    );
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
    let message = verify_checksum_manifest(&target, &mut after).unwrap_err();
    assert_eq!(
        message,
        Totals {
            matched: 1,
            changed: 1,
            missing: 1,
            errors: 0,
        }
        .message()
    );
    for (row, path, reason) in [
        (&after.results.items[0], &unchanged, "SHA-256 совпала"),
        (
            &after.results.items[1],
            &edited,
            "Файл изменён (SHA-256 не совпала)",
        ),
        (&after.results.items[2], &absent, "Файл отсутствует"),
    ] {
        assert_eq!(row.source, path.display());
        assert_eq!(row.reason.as_deref(), Some(text(reason)));
    }
    assert_eq!(fs::read(unchanged.path()).unwrap(), b"abc");
    assert_eq!(fs::read(edited.path()).unwrap(), b"xyz");
    assert_eq!(fs::read(target.path()).unwrap(), manifest);
}

#[test]
fn checksum_manifest_distinguishes_matches_changes_and_missing_files() {
    let root = tempfile::tempdir().unwrap();
    checksum_round_trip(&Location::Local(root.path().to_owned()));
}

#[test]
fn checksum_parser_validates_paths_and_normalized_duplicates_before_reading_files() {
    let cancel = Cancellation::default();
    let tagged = format!(
        "\u{feff}# SHA-256\r\nSHA256 (./nested/data (copy)) = {}\r\n",
        ABC.to_uppercase()
    );
    let entries = manifest::parse(tagged.as_bytes(), &cancel).unwrap();
    assert_eq!(entries[0].path, Path::new("nested/data (copy)"));
    assert_eq!(entries[0].digest[0..4], [0xba, 0x78, 0x16, 0xbf]);
    for invalid in [
        format!("{ABC}  ../outside.txt\n"),
        format!("{ABC}  /outside.txt\n"),
        format!("{ABC}  sub/../outside.txt\n"),
        format!("{ABC}  dir/\n"),
        format!("{ABC}  .\n"),
        format!("{ABC}  nested/a\n{ABC} *./nested/a\n"),
        format!("\\{ABC}  escaped\\qname\n"),
        format!("{}x  invalid-digest\n", &ABC[..63]),
        format!("{ABC}  nul\0name\n"),
    ] {
        assert!(
            manifest::parse(invalid.as_bytes(), &cancel).is_err(),
            "{invalid:?}"
        );
    }
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
    let message = verify_checksum_manifest(&manifest, &mut progress).unwrap_err();
    assert_eq!(
        message,
        Totals {
            matched: 1,
            changed: 0,
            missing: 0,
            errors: 2,
        }
        .message()
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
    create_checksum_manifest(
        &directory,
        "SHA256SUMS",
        std::slice::from_ref(&source),
        &mut progress,
    )
    .unwrap();
    assert!(progress.results.cancelled);
    assert_eq!(fs::read(source.path()).unwrap(), b"abc");
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}
