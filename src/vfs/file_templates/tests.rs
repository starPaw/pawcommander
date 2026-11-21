use super::*;
use crate::operations::Cancellation;
use std::{fs, os::unix::fs::symlink};

fn draft(value: &str, encoding: TextEncoding) -> Source {
    Source::Text {
        value: value.into(),
        encoding,
    }
}

fn job(directory: &Location, name: &str, source: Source) -> NewFile {
    NewFile {
        directory: directory.clone(),
        name: name.into(),
        source,
    }
}

#[test]
fn writes_explicit_text_encodings_and_rejects_lossy_output() {
    let root = tempfile::tempdir().unwrap();
    let directory = Location::Local(root.path().into());
    let cases = [
        (TextEncoding::Utf8, "ёж", "ёж".as_bytes()),
        (TextEncoding::Utf8Bom, "é", b"\xef\xbb\xbf\xc3\xa9"),
        (
            TextEncoding::Utf16Le,
            "Я🙂",
            b"\xff\xfe\x2f\x04\x3d\xd8\x42\xde",
        ),
        (
            TextEncoding::Utf16Be,
            "Я🙂",
            b"\xfe\xff\x04\x2f\xd8\x3d\xde\x42",
        ),
        (
            TextEncoding::Windows1251,
            "Привет",
            b"\xcf\xf0\xe8\xe2\xe5\xf2",
        ),
        (TextEncoding::Windows1252, "Café €", b"Caf\xe9 \x80"),
    ];
    for (index, (encoding, text, expected)) in cases.into_iter().enumerate() {
        let name = format!("{index}.txt");
        job(&directory, &name, draft(text, encoding))
            .create(&mut Progress::default())
            .unwrap();
        assert_eq!(fs::read(root.path().join(name)).unwrap(), expected);
    }
    assert!(job(
        &directory,
        "unsupported.txt",
        draft("🙂", TextEncoding::Windows1251)
    )
    .create(&mut Progress::default())
    .is_err());
    assert!(
        job(&directory, "../outside", draft("x", TextEncoding::Utf8))
            .create(&mut Progress::default())
            .is_err()
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), cases.len());
}

#[test]
fn saved_templates_survive_a_profile_round_trip_and_source_removal() {
    let root = tempfile::tempdir().unwrap();
    let directory = Location::Local(root.path().into());
    let source = directory.join("sample.bin");
    let bytes = b"\xff\xfe\x2f\x04\x00\xd8\x0d\x00\x0a\x00";
    fs::write(source.path(), bytes).unwrap();
    let snapshot = save_template(
        "Fixture".into(),
        "sample.bin".into(),
        Source::File(source.clone()),
        &mut Progress::default(),
    )
    .unwrap();
    let mut library = Library::default();
    assert_eq!(library.insert(snapshot).unwrap(), 0);
    fs::remove_file(source.path()).unwrap();
    let settings = ron::to_string(&library).unwrap();
    let library: Library = ron::from_str(&settings).unwrap();
    assert_eq!(library.templates[0].filename, "sample.bin");
    assert!(library.check_title("fixture").is_err());
    job(
        &directory,
        "from-template.bin",
        library.templates[0].source(),
    )
    .create(&mut Progress::default())
    .unwrap();
    assert_eq!(
        fs::read(directory.join("from-template.bin").path()).unwrap(),
        bytes
    );
}

#[test]
fn collision_and_cancellation_preserve_existing_items_and_clean_staging() {
    let root = tempfile::tempdir().unwrap();
    let directory = Location::Local(root.path().into());
    fs::write(root.path().join("original"), b"original bytes").unwrap();
    symlink("original", root.path().join("link")).unwrap();
    let source = || draft("draft", TextEncoding::Utf8);
    assert!(job(&directory, "link", source())
        .create(&mut Progress::default())
        .is_err());
    assert_eq!(
        fs::read_link(root.path().join("link")).unwrap(),
        Path::new("original")
    );

    let payload_is_staged = |folder: &Path| {
        fs::read_dir(folder).unwrap().any(|entry| {
            let entry = entry.unwrap();
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".pawcommander-transfer-")
                && entry.path().join("payload").is_file()
        })
    };
    let folder = root.path().to_owned();
    let competitor = folder.join("taken.txt");
    let existing = competitor.clone();
    let racing = Cancellation::at_checkpoint(move || {
        if payload_is_staged(&folder) && !competitor.exists() {
            fs::write(&competitor, b"competing create").unwrap();
        }
        false
    });
    assert!(job(&directory, "taken.txt", source())
        .create(&mut Progress::with_cancellation(racing))
        .is_err());
    assert_eq!(fs::read(existing).unwrap(), b"competing create");

    let folder = root.path().to_owned();
    let stopping = Cancellation::at_checkpoint(move || payload_is_staged(&folder));
    assert!(job(&directory, "stopped.txt", source())
        .create(&mut Progress::with_cancellation(stopping))
        .is_err());
    assert!(!root.path().join("stopped.txt").exists());
    assert_eq!(
        fs::read(root.path().join("original")).unwrap(),
        b"original bytes"
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 3);
}

pub(in crate::vfs) fn check_sftp(root: &Location) {
    let directory = crate::vfs::create_directory(root, "file-templates").unwrap();
    let original = directory.join("legacy.txt");
    let existing = directory.join("created.txt");
    let bytes = b"\xcf\xf0\xe8\xe2\xe5\xf2\r\n";
    fs::write(original.path(), bytes).unwrap();
    let template = save_template(
        "Remote template".into(),
        "legacy.txt".into(),
        Source::File(original.clone()),
        &mut Progress::default(),
    )
    .unwrap();
    job(&directory, "created.txt", template.source())
        .create(&mut Progress::default())
        .unwrap();
    assert_eq!(fs::read(existing.path()).unwrap(), bytes);
    assert_eq!(fs::read(original.path()).unwrap(), bytes);
    assert!(job(
        &directory,
        "created.txt",
        draft("different", TextEncoding::Utf8)
    )
    .create(&mut Progress::default())
    .is_err());
    assert_eq!(fs::read(existing.path()).unwrap(), bytes);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    println!("New file templates over SFTP preserved the legacy bytes and existing destination");
}
