use super::*;
use crate::{archive::Format, operations::Cancellation, vfs::create_archive};
use flate2::{write::DeflateEncoder, Compression};
use std::{io::Write, path::Path};

fn unexpected_conflict(_: Conflict) -> Result<Answer, String> {
    panic!("unexpected conflict prompt")
}

fn no_staging(directory: &Location) {
    assert!(fs::read_dir(directory.path())
        .unwrap()
        .map(Result::unwrap)
        .all(|entry| {
            !entry
                .file_name()
                .as_encoded_bytes()
                .starts_with(b".pawcommander-")
        }));
}

#[test]
fn extracts_only_the_selected_paths_from_both_formats() {
    let temp = tempfile::tempdir().unwrap();
    let input = Location::Local(temp.path().to_owned());
    let tree = input.join("Original");
    fs::create_dir_all(tree.join("Nested/Empty").path()).unwrap();
    let sample = b"Multibyte selection and a buffer boundary\n".repeat(4000);
    fs::write(tree.join("Nested/данные.txt").path(), &sample).unwrap();
    fs::write(tree.join("zero.txt").path(), b"").unwrap();
    fs::write(tree.join("ignored.txt").path(), b"not selected").unwrap();
    for (format, extension) in [(Format::Zip, "zip"), (Format::Tar, "tar")] {
        let mut progress = Progress::default();
        create_archive(
            std::slice::from_ref(&tree),
            &input,
            "source",
            format,
            None,
            &mut progress,
        )
        .unwrap();
        let source = input.join(format!("source.{extension}"));
        let initial = fs::read(source.path()).unwrap();
        let original = source.browse_archive().unwrap().join("Original");
        let output = input.join(format!("out-{extension}"));
        fs::create_dir(output.path()).unwrap();
        extract_archive(
            &[original.join("Nested"), original.join("zero.txt")],
            &output,
            None,
            &mut progress,
            unexpected_conflict,
        )
        .unwrap();
        assert_eq!(
            fs::read(output.join("Nested/данные.txt").path()).unwrap(),
            sample
        );
        assert!(output.join("Nested/Empty").path().is_dir());
        assert!(fs::read(output.join("zero.txt").path()).unwrap().is_empty());
        assert!(!output.join("Original").path().exists());
        assert!(!output.join("ignored.txt").path().exists());
        assert_eq!(fs::read(source.path()).unwrap(), initial);
        assert!(progress
            .results
            .items
            .iter()
            .all(|item| item.outcome == Outcome::Completed));
        no_staging(&output);
    }
}

#[test]
fn password_zip_uses_aes_and_keeps_secrets_out_of_results() {
    let temp = tempfile::tempdir().unwrap();
    let root = Location::Local(temp.path().into());
    let folder = root.join("Folder");
    fs::create_dir_all(folder.join("Empty").path()).unwrap();
    let bytes = b"encrypted ZIP bytes cross multiple output buffers\n".repeat(7000);
    fs::write(folder.join("данные.txt").path(), &bytes).unwrap();
    fs::write(folder.join("zero").path(), b"").unwrap();
    let password = " fixture-архив-2026 secret ";
    let mut progress = Progress::default();
    create_archive(
        std::slice::from_ref(&folder),
        &root,
        "locked",
        Format::Zip,
        Some(password),
        &mut progress,
    )
    .unwrap();
    let archive = root.join("locked.zip");
    let mut buffer = vec![0; rawzip::RECOMMENDED_BUFFER_SIZE];
    let raw =
        rawzip::ZipArchive::from_seekable(fs::File::open(archive.path()).unwrap(), &mut buffer)
            .unwrap();
    let mut entries = raw.entries(&mut buffer);
    let mut encrypted = 0;
    while let Some(entry) = entries.next_entry().unwrap() {
        if !entry.is_dir() {
            assert!(entry.flags().is_encrypted());
            assert_eq!(entry.compression_method(), rawzip::CompressionMethod::AES);
            assert!(entry
                .extra_fields()
                .any(|(id, data)| id == rawzip::extra_fields::ExtraFieldId::AES
                    && data.get(4) == Some(&3)));
            encrypted += 1;
        }
    }
    assert_eq!(encrypted, 2);

    let dest = root.join("out");
    fs::create_dir(dest.path()).unwrap();
    let selection = archive.browse_archive().unwrap().join("Folder");
    let missing = extract_archive(
        std::slice::from_ref(&selection),
        &dest,
        None,
        &mut progress,
        unexpected_conflict,
    );
    assert!(missing.as_ref().is_err_and(
        |message| message.contains(crate::i18n::text!("Укажите пароль для распаковки ZIP"))
    ));
    assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 0);

    // AES ZIP has a short password verifier. Pick a definitely rejected test
    // password so this regression never depends on a random salt collision.
    let mut reference = zip::ZipArchive::new(fs::File::open(archive.path()).unwrap()).unwrap();
    let index = reference.index_for_name("Folder/zero").unwrap();
    let wrong = (1..=8)
        .map(|n| format!("wrong fixture secret {n}"))
        .find(|guess| {
            matches!(
                reference.by_index_decrypt(index, guess.as_bytes()),
                Err(zip::result::ZipError::InvalidPassword)
            )
        })
        .unwrap();
    let part = selection.join("zero");
    let wrong_result = extract_archive(
        &[part],
        &dest,
        Some(&wrong),
        &mut progress,
        unexpected_conflict,
    );
    assert!(wrong_result
        .as_ref()
        .is_err_and(|message| message.contains(crate::i18n::text!("Неверный пароль ZIP"))));
    let failed_record = std::mem::take(&mut progress.results)
        .finish("Extract".into(), 0, &wrong_result)
        .unwrap();
    let json = serde_json::to_string(&failed_record).unwrap();
    assert!(!json.contains(&wrong) && !json.contains(password));
    assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 0);

    let good = extract_archive(
        &[selection],
        &dest,
        Some(password),
        &mut progress,
        unexpected_conflict,
    );
    assert!(good.is_ok(), "{good:?}");
    assert_eq!(
        fs::read(dest.join("Folder/данные.txt").path()).unwrap(),
        bytes
    );
    assert!(dest.join("Folder/Empty").path().is_dir());
    assert_eq!(
        fs::metadata(dest.join("Folder/zero").path()).unwrap().len(),
        0
    );
    let success_record = progress.results.finish("Extract".into(), 0, &good).unwrap();
    assert!(!serde_json::to_string(&success_record)
        .unwrap()
        .contains(password));
    no_staging(&dest);
}

#[test]
fn a_bad_aes_authenticator_blocks_replacement_even_for_an_empty_member() {
    let temp = tempfile::tempdir().unwrap();
    let root = Location::Local(temp.path().into());
    let source = root.join("zero");
    fs::write(source.path(), b"").unwrap();
    let password = "disposable AES fixture password";
    let mut progress = Progress::default();
    create_archive(
        std::slice::from_ref(&source),
        &root,
        "protected",
        Format::Zip,
        Some(password),
        &mut progress,
    )
    .unwrap();
    let archive = root.join("protected.zip");
    let mut bytes = fs::read(archive.path()).unwrap();
    let last_auth_byte = {
        let zip = rawzip::ZipArchive::from_slice(&bytes).unwrap();
        let mut entries = zip.entries();
        let member = entries.next_entry().unwrap().unwrap();
        (zip.get_entry(member.wayfinder())
            .unwrap()
            .compressed_data_range()
            .1
            - 1) as usize
    };
    bytes[last_auth_byte] ^= 1;
    fs::write(archive.path(), &bytes).unwrap();
    let before = b"existing destination";
    fs::write(source.path(), before).unwrap();
    let entry = archive.browse_archive().unwrap().join("zero");

    let mut saw_conflict = false;
    let result = extract_archive(&[entry], &root, Some(password), &mut progress, |conflict| {
        assert!(conflict.can_replace);
        saw_conflict = true;
        Ok(Answer {
            decision: Decision::Replace,
            apply_to_group: false,
        })
    });
    assert!(saw_conflict);
    assert!(result
        .as_ref()
        .is_err_and(|message| message.contains(crate::i18n::text!(
            "Неверный пароль или повреждены данные ZIP"
        ))));
    assert_eq!(progress.results.items[0].outcome, Outcome::Failed);
    assert_eq!(fs::read(source.path()).unwrap(), before);
    assert_eq!(fs::read(archive.path()).unwrap(), bytes);
    no_staging(&root);
}

#[test]
fn reads_legacy_zipcrypto_and_keeps_tar_unencrypted() {
    use zip::unstable::write::FileOptionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = Location::Local(temp.path().into());
    let archive = root.join("legacy.zip");
    let mut writer = zip::ZipWriter::new(fs::File::create(archive.path()).unwrap());
    let password = "legacy fixture password";
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .with_deprecated_encryption(password.as_bytes())
        .unwrap();
    writer.start_file("legacy.txt", options).unwrap();
    writer.write_all(b"legacy bytes").unwrap();
    writer.finish().unwrap();
    let file = archive.browse_archive().unwrap().join("legacy.txt");
    let mut progress = Progress::default();
    extract_archive(
        &[file],
        &root,
        Some(password),
        &mut progress,
        unexpected_conflict,
    )
    .unwrap();
    let output = root.join("legacy.txt");
    assert_eq!(fs::read(output.path()).unwrap(), b"legacy bytes");
    assert!(create_archive(
        &[output],
        &root,
        "unsupported",
        Format::Tar,
        Some(password),
        &mut progress
    )
    .is_err());
    assert!(!root.join("unsupported.tar").path().exists());
    no_staging(&root);
}

#[test]
fn conflicting_files_can_be_replaced_or_kept_with_reserved_names() {
    let temp = tempfile::tempdir().unwrap();
    let directory = Location::Local(temp.path().into());
    let archive = directory.join("source.tar");
    let mut tar = tar::Builder::new(fs::File::create(archive.path()).unwrap());
    tar_file(&mut tar, "replace.txt", b"replacement");
    tar_file(&mut tar, "copy.txt", b"second");
    tar_file(&mut tar, "copy (2).txt", b"reserved");
    tar.into_inner().unwrap();
    fs::write(directory.join("replace.txt").path(), b"before").unwrap();
    fs::write(directory.join("copy.txt").path(), b"before").unwrap();
    let root = archive.browse_archive().unwrap();
    let mut choices = [Decision::Replace, Decision::KeepBoth].into_iter();
    let mut progress = Progress::default();
    extract_archive(
        &[
            root.join("replace.txt"),
            root.join("copy.txt"),
            root.join("copy (2).txt"),
        ],
        &directory,
        None,
        &mut progress,
        |conflict| {
            assert!(conflict.can_replace);
            assert_eq!(conflict.count, 2);
            Ok(Answer {
                decision: choices.next().unwrap(),
                apply_to_group: false,
            })
        },
    )
    .unwrap();
    assert_eq!(
        fs::read(directory.join("replace.txt").path()).unwrap(),
        b"replacement"
    );
    assert_eq!(
        fs::read(directory.join("copy.txt").path()).unwrap(),
        b"before"
    );
    assert_eq!(
        fs::read(directory.join("copy (2).txt").path()).unwrap(),
        b"reserved"
    );
    assert_eq!(
        fs::read(directory.join("copy (3).txt").path()).unwrap(),
        b"second"
    );
    no_staging(&directory);
}

#[test]
fn staged_output_is_withheld_for_crc_or_length_mismatch_and_volume_limit() {
    let temp = tempfile::tempdir().unwrap();
    let directory = Location::Local(temp.path().into());
    let file = directory.join("item.txt");
    let before = b"existing destination";
    let data = b"archive payload that must be checked";
    fs::write(file.path(), before).unwrap();

    for (claim, crc, deflate, expected_prompts) in [
        (data.len() as u64, rawzip::crc32(data) ^ 1, false, 1),
        (12, rawzip::crc32(&data[..12]), true, 1),
        (32 * 1024 * 1024 * 1024 + 1, rawzip::crc32(data), true, 0),
    ] {
        let archive = directory.join("validated.zip");
        zip_file(archive.path(), data, claim, crc, deflate);
        let mut prompts = 0;
        let mut progress = Progress::default();
        let source = archive.browse_archive().unwrap().join("item.txt");
        assert!(
            extract_archive(&[source], &directory, None, &mut progress, |conflict| {
                assert!(conflict.can_replace);
                prompts += 1;
                Ok(Answer {
                    decision: Decision::Replace,
                    apply_to_group: false,
                })
            })
            .is_err()
        );
        assert_eq!(prompts, expected_prompts);
        assert_eq!(fs::read(file.path()).unwrap(), before);
        no_staging(&directory);
    }
}

#[test]
fn extracting_an_entry_cannot_replace_the_archive_itself() {
    let temp = tempfile::tempdir().unwrap();
    let directory = Location::Local(temp.path().into());
    let source = directory.join("original.tar");
    let mut tar = tar::Builder::new(fs::File::create(source.path()).unwrap());
    tar_file(&mut tar, "original.tar", b"ordinary archive member");
    tar.into_inner().unwrap();
    let before = fs::read(source.path()).unwrap();
    let nested = source.browse_archive().unwrap().join("original.tar");
    let mut progress = Progress::default();
    assert!(
        extract_archive(&[nested], &directory, None, &mut progress, |conflict| {
            assert!(!conflict.can_replace);
            Ok(Answer {
                decision: Decision::Replace,
                apply_to_group: false,
            })
        })
        .is_err()
    );
    assert_eq!(fs::read(source.path()).unwrap(), before);
    no_staging(&directory);
}

#[test]
fn rejects_untrusted_paths_and_symlink_entries_before_any_write() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Location::Local(temp.path().into());
    let output = parent.join("destination");
    fs::create_dir(output.path()).unwrap();
    let outside = parent.join("outside.txt");
    fs::write(outside.path(), b"do not overwrite").unwrap();
    let archive = parent.join("test.tar");
    let root = archive.browse_archive().unwrap();

    let mut tar = tar::Builder::new(fs::File::create(archive.path()).unwrap());
    tar_file(&mut tar, "ordinary.txt", b"ordinary");
    let mut header = tar::Header::new_gnu();
    let invalid_path = b"../outside.txt";
    header.as_gnu_mut().unwrap().name[..invalid_path.len()].copy_from_slice(invalid_path);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(0);
    header.set_cksum();
    tar.append(&header, std::io::empty()).unwrap();
    tar.into_inner().unwrap();
    let mut progress = Progress::default();
    assert!(extract_archive(
        &[root.join("ordinary.txt")],
        &output,
        None,
        &mut progress,
        unexpected_conflict
    )
    .is_err());
    assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);

    let mut tar = tar::Builder::new(fs::File::create(archive.path()).unwrap());
    tar_file(&mut tar, "Tree/ordinary.txt", b"ordinary");
    let mut header = tar::Header::new_gnu();
    header.set_size(0);
    header.set_entry_type(tar::EntryType::Symlink);
    tar.append_link(&mut header, "Tree/Alias", "../../outside.txt")
        .unwrap();
    tar.into_inner().unwrap();
    assert!(extract_archive(
        &[root.join("Tree")],
        &output,
        None,
        &mut progress,
        unexpected_conflict
    )
    .is_err());
    assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);
    assert_eq!(fs::read(outside.path()).unwrap(), b"do not overwrite");
}

#[cfg(unix)]
#[test]
fn a_symlink_in_the_destination_is_not_used_as_an_output_directory() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Location::Local(temp.path().into());
    let destination = parent.join("destination");
    let outside = parent.join("outside");
    fs::create_dir(destination.path()).unwrap();
    fs::create_dir(outside.path()).unwrap();
    std::os::unix::fs::symlink(outside.path(), destination.join("Folder").path()).unwrap();
    let archive = parent.join("tree.tar");
    let mut tar = tar::Builder::new(fs::File::create(archive.path()).unwrap());
    tar_file(&mut tar, "Folder/file.txt", b"selected");
    tar.into_inner().unwrap();
    let tree = archive.browse_archive().unwrap().join("Folder");
    let mut progress = Progress::default();
    extract_archive(&[tree], &destination, None, &mut progress, |conflict| {
        assert!(!conflict.can_replace);
        Ok(Answer {
            decision: Decision::KeepBoth,
            apply_to_group: false,
        })
    })
    .unwrap();
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    assert_eq!(
        fs::read_link(destination.join("Folder").path()).unwrap(),
        outside.path()
    );
    assert_eq!(
        fs::read(destination.join("Folder (2)/file.txt").path()).unwrap(),
        b"selected"
    );
    no_staging(&destination);
}

#[test]
fn stopping_a_stream_keeps_completed_entries_and_removes_the_active_payload() {
    const SIZE: usize = 1024 * 1024;
    let temp = tempfile::tempdir().unwrap();
    let directory = Location::Local(temp.path().into());
    let archive = directory.join("source.tar");
    let mut tar = tar::Builder::new(fs::File::create(archive.path()).unwrap());
    tar_file(&mut tar, "first.txt", b"completed");
    tar_file(&mut tar, "large.bin", &vec![23; SIZE]);
    tar_file(&mut tar, "last.txt", b"still pending");
    tar.into_inner().unwrap();
    let root = archive.browse_archive().unwrap();
    let watch = directory.path().to_owned();
    let cancellation = Cancellation::at_checkpoint(move || {
        fs::read_dir(&watch)
            .unwrap()
            .map(Result::unwrap)
            .any(|entry| {
                entry
                    .file_name()
                    .as_encoded_bytes()
                    .starts_with(b".pawcommander-transfer-")
                    && fs::metadata(entry.path().join("payload"))
                        .is_ok_and(|meta| (64 * 1024..SIZE as u64).contains(&meta.len()))
            })
    });
    let mut progress = Progress::with_cancellation(cancellation);
    extract_archive(
        &[
            root.join("first.txt"),
            root.join("large.bin"),
            root.join("last.txt"),
        ],
        &directory,
        None,
        &mut progress,
        unexpected_conflict,
    )
    .unwrap();
    assert_eq!(
        fs::read(directory.join("first.txt").path()).unwrap(),
        b"completed"
    );
    assert!(!directory.join("large.bin").path().exists());
    assert!(!directory.join("last.txt").path().exists());
    assert!(progress.results.cancelled);
    assert_eq!(
        progress
            .results
            .items
            .iter()
            .map(|item| item.outcome)
            .collect::<Vec<_>>(),
        [Outcome::Completed, Outcome::Cancelled, Outcome::NotStarted]
    );
    no_staging(&directory);
}

fn tar_file(tar: &mut tar::Builder<fs::File>, name: &str, data: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    tar.append_data(&mut header, name, data).unwrap();
}

fn zip_file(path: &Path, data: &[u8], size: u64, crc: u32, deflate: bool) {
    let mut zip = rawzip::ZipArchiveWriter::new(fs::File::create(path).unwrap());
    let method = if deflate {
        rawzip::CompressionMethod::DEFLATE
    } else {
        rawzip::CompressionMethod::STORE
    };
    let (mut entry, _) = zip
        .new_file("item.txt")
        .compression_method(method)
        .start()
        .unwrap();
    if deflate {
        let mut encoder = DeflateEncoder::new(&mut entry, Compression::fast());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap();
    } else {
        entry.write_all(data).unwrap();
    }
    entry
        .finish(rawzip::DataDescriptorOutput::new(crc, size))
        .unwrap();
    zip.finish().unwrap();
}
