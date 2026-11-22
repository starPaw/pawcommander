use super::*;
use crate::{
    file_error::{AccessCause, Failure},
    operations::Cancellation,
};
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

fn template_error(error: TransferError) -> Box<FileTemplateError> {
    let TransferError::Failed(Failure::FileTemplate(error)) = error else {
        panic!("expected a template error, got {error:?}");
    };
    error
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
        let created = job(&directory, &name, draft(text, encoding))
            .create(&mut Progress::default())
            .unwrap();
        assert_eq!(created.target, directory.join(&name));
        assert!(created.cleanup_warning.is_none());
        assert_eq!(fs::read(root.path().join(name)).unwrap(), expected);
    }
    let error = job(
        &directory,
        "unsupported.txt",
        draft("🙂", TextEncoding::Windows1251),
    )
    .create(&mut Progress::default())
    .unwrap_err();
    assert!(
        matches!(error, TransferError::Failed(Failure::FileTemplate(reason))
        if matches!(reason.as_ref(), FileTemplateError::UnrepresentableCharacters(TextEncoding::Windows1251)))
    );
    assert!(matches!(
        job(&directory, "../outside", draft("x", TextEncoding::Utf8)).create(&mut Progress::default()),
        Err(TransferError::Failed(Failure::Path(PathError::InvalidName(name)))) if name == "../outside"
    ));
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
    assert!(
        matches!(template_error(library.check_new("fixture").unwrap_err()).as_ref(),
        FileTemplateError::DuplicateTitle(title) if title == "Fixture")
    );
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
fn unavailable_template_sources_keep_the_path_or_decoding_cause() {
    use std::error::Error;

    let root = tempfile::tempdir().unwrap();
    let directory = Location::Local(root.path().into());
    let source = directory.join("template.bin");
    let target = "draft.bin";
    let error = job(&directory, target, Source::File(source.clone()))
        .create(&mut Progress::default())
        .unwrap_err();
    assert!(
        matches!(error, TransferError::Failed(Failure::FileTemplate(reason))
        if matches!(reason.as_ref(), FileTemplateError::SourceMissing { source: path, cause }
            if path == &source && matches!(&cause.cause, AccessCause::Local(error)
                if error.raw_os_error() == Some(libc::ENOENT))))
    );

    let saved = SavedTemplate {
        title: "Unreadable template".into(),
        filename: "draft.bin".into(),
        payload: "!!!!".into(),
    };
    let error = job(&directory, target, saved.source())
        .create(&mut Progress::default())
        .unwrap_err();
    let reason = template_error(error);
    assert!(matches!(reason.as_ref(),
        FileTemplateError::InvalidSavedPayload { title, .. } if title == &saved.title));
    assert!(reason.source().unwrap().is::<base64::DecodeError>());
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[test]
fn library_validation_distinguishes_invalid_names_and_preserves_entries_at_both_limits() {
    let template = |title: String, payload: Arc<str>| SavedTemplate {
        title,
        filename: "draft.txt".into(),
        payload,
    };
    let mut library = Library::default();
    assert!(matches!(
        *template_error(library.check_new("  ").unwrap_err()),
        FileTemplateError::EmptyTitle
    ));
    assert!(matches!(
        *template_error(library.check_new("a\nb").unwrap_err()),
        FileTemplateError::TitleHasControl
    ));
    assert!(matches!(
        *template_error(
            library
                .check_new(&"ř".repeat(MAX_TITLE_CHARS + 1))
                .unwrap_err()
        ),
        FileTemplateError::TitleTooLong(MAX_TITLE_CHARS)
    ));
    for index in 0..MAX_TEMPLATES {
        library
            .insert(template(format!("Template {index}"), "".into()))
            .unwrap();
    }
    let persisted = ron::to_string(&library).unwrap();
    assert!(matches!(
        *template_error(library.check_new("Another").unwrap_err()),
        FileTemplateError::LibraryCountLimit(MAX_TEMPLATES)
    ));
    assert!(matches!(
        *template_error(
            library
                .insert(template("Another".into(), "".into()))
                .unwrap_err()
        ),
        FileTemplateError::LibraryCountLimit(MAX_TEMPLATES)
    ));
    assert_eq!(ron::to_string(&library).unwrap(), persisted);

    let payload: Arc<str> = "A".repeat(MAX_LIBRARY_BYTES / 4).into();
    let mut library = Library::default();
    for index in 0..4 {
        library
            .insert(template(format!("Large {index}"), payload.clone()))
            .unwrap();
    }
    assert!(library.check_new("Another").is_ok());
    assert!(matches!(
        *template_error(
            library
                .insert(template("Another".into(), "eA==".into()))
                .unwrap_err()
        ),
        FileTemplateError::LibrarySizeLimit(MAX_LIBRARY_BYTES)
    ));
    assert_eq!(library.templates.len(), 4);
    assert!(library
        .templates
        .iter()
        .all(|saved| Arc::ptr_eq(&saved.payload, &payload)));
}

#[test]
fn saving_a_template_keeps_the_title_source_cause_and_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let source = Location::Local(root.path().join("missing.txt"));
    let title = "Copy to keep";
    let capture = |progress: &mut Progress| {
        save_template(
            title.into(),
            "draft.txt".into(),
            Source::File(source.clone()),
            progress,
        )
    };
    let reason = template_error(capture(&mut Progress::default()).err().unwrap());
    let FileTemplateError::Save {
        title: captured,
        cause,
    } = *reason
    else {
        panic!("expected save context");
    };
    assert_eq!(captured, title);
    assert!(matches!(template_error(*cause).as_ref(),
        FileTemplateError::SourceMissing { source: selected, cause }
            if selected == &source && matches!(&cause.cause, AccessCause::Local(error)
                if error.raw_os_error() == Some(libc::ENOENT))));

    let stop = Cancellation::at_checkpoint({
        let mut calls = 0;
        move || {
            calls += 1;
            calls == 3
        }
    });
    assert!(matches!(
        capture(&mut Progress::with_cancellation(stop)),
        Err(TransferError::Cancelled)
    ));

    let too_long = draft(&"x".repeat(MAX_BYTES + 1), TextEncoding::Utf8);
    let error = save_template(
        title.into(),
        "draft.txt".into(),
        too_long,
        &mut Progress::default(),
    )
    .err()
    .unwrap();
    let reason = template_error(error);
    assert!(
        matches!(reason.as_ref(), FileTemplateError::Save { title: attempted, cause }
        if attempted == title && matches!(cause.as_ref(), TransferError::Failed(Failure::FileTemplate(reason))
            if matches!(reason.as_ref(), FileTemplateError::TooLarge(None))))
    );
}

#[test]
fn collision_and_cancellation_preserve_existing_items_and_clean_staging() {
    let root = tempfile::tempdir().unwrap();
    let directory = Location::Local(root.path().canonicalize().unwrap());
    fs::write(root.path().join("original"), b"original bytes").unwrap();
    symlink("original", root.path().join("link")).unwrap();
    let source = || draft("draft", TextEncoding::Utf8);
    let error = job(&directory, "link", source())
        .create(&mut Progress::default())
        .unwrap_err();
    assert!(
        matches!(error, TransferError::Failed(Failure::Mutation(reason))
        if matches!(reason.as_ref(), MutationError::DestinationExists(path) if path == &directory.join("link")))
    );
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
    let error = job(&directory, "taken.txt", source())
        .create(&mut Progress::with_cancellation(racing))
        .unwrap_err();
    assert!(
        matches!(error, TransferError::Failed(Failure::Mutation(reason))
        if matches!(reason.as_ref(), MutationError::DestinationExists(path) if path == &directory.join("taken.txt")))
    );
    assert_eq!(fs::read(existing).unwrap(), b"competing create");

    let folder = root.path().to_owned();
    let stopping = Cancellation::at_checkpoint(move || payload_is_staged(&folder));
    let mut progress = Progress::with_cancellation(stopping);
    assert!(matches!(
        job(&directory, "stopped.txt", source()).create(&mut progress),
        Err(TransferError::Cancelled)
    ));
    assert!(progress.results.cancelled);
    assert_eq!(
        progress.results.root_outcomes(),
        [(directory.join("stopped.txt"), Outcome::Cancelled)]
    );
    assert!(!root.path().join("stopped.txt").exists());
    assert_eq!(
        fs::read(root.path().join("original")).unwrap(),
        b"original bytes"
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 3);
}

pub(in crate::vfs) fn check_sftp(root: &Location) {
    let directory = crate::vfs::create_directory(root, "file-templates").unwrap();
    let directory = directory.with_path(super::super::transfer::canonical(&directory).unwrap());
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
    let error = job(
        &directory,
        "created.txt",
        draft("different", TextEncoding::Utf8),
    )
    .create(&mut Progress::default())
    .unwrap_err();
    assert!(
        matches!(&error, TransferError::Failed(Failure::Mutation(reason))
        if matches!(reason.as_ref(), MutationError::DestinationExists(path) if path == &existing)),
        "Unexpected result for {existing}: {error:?}"
    );
    assert_eq!(fs::read(existing.path()).unwrap(), bytes);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    let absent = directory.join("vanished.txt");
    let error = save_template(
        "Missing remote template".into(),
        "draft.txt".into(),
        Source::File(absent.clone()),
        &mut Progress::default(),
    )
    .err()
    .unwrap();
    let FileTemplateError::Save { cause, .. } = *template_error(error) else {
        panic!("expected the failed save of a remote template");
    };
    assert!(matches!(template_error(*cause).as_ref(),
        FileTemplateError::SourceMissing { source, cause }
            if source == &absent && matches!(&cause.cause, AccessCause::Sftp(error)
                if error.code() == ssh2::ErrorCode::SFTP(2))));
    println!("New file templates over SFTP preserved the legacy bytes and existing destination");
}
