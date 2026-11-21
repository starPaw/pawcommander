use super::extraction::{copy_exact, Extraction};
use super::{
    invalid, member_path, scan_zip, too_large, Contents, Format, MetadataReader, MAX_ENTRIES,
    MAX_INDEX_BYTES, MAX_METADATA_BYTES,
};
use crate::{
    i18n::text,
    operations::{Progress, TransferError},
    result_history::Outcome,
    vfs::Location,
};
use std::{cell::Cell, io, path::Path};

pub(crate) fn verify_archive(
    path: &Path,
    password: Option<&str>,
    progress: &mut Progress,
) -> Result<String, String> {
    let source = Location::Local(path.to_owned());
    progress.results.plan([(source.clone(), None)]);
    progress.results.begin(&source);
    progress.item(path);
    let format = Format::from_path(path).ok_or_else(|| text!("Поддерживаются архивы ZIP и TAR"))?;
    let mut report = Report {
        source: &source,
        progress,
        passed: 0,
        damaged: 0,
        unverified: 0,
    };
    let result = report.check(path, format, password);
    let summary = crate::tr_format!(
        "Проверено записей: {}. Ошибки в записях: {}. Без проверки: {}.",
        "Verified entries: {}. Entry errors: {}. Unverified: {}.",
        report.passed,
        report.damaged,
        report.unverified
    );
    let cancelled = report.progress.cancellation().is_requested()
        || result.as_ref().is_err_and(TransferError::is_cancelled_io);
    if result.is_ok() && !cancelled {
        report.progress.results.finish_diagnostics(&source);
    }
    let reason = result
        .as_ref()
        .err()
        .filter(|_| !cancelled)
        .map(ToString::to_string);
    let details = text!("Подробности в журнале результатов.");
    let (outcome, message) = if cancelled {
        report.progress.results.cancelled = true;
        (
            Outcome::Cancelled,
            crate::tr_format!(
                "Проверка архива остановлена. {summary}",
                "Archive check stopped. {summary}"
            ),
        )
    } else if let Err(error) = result {
        (
            Outcome::Failed,
            crate::tr_format!(
                "Проверка архива прервана: {error}. {summary} {details}",
                "Archive check could not finish: {error}. {summary} {details}"
            ),
        )
    } else if report.damaged != 0 {
        (Outcome::Failed, format!("{summary} {details}"))
    } else if report.unverified != 0 {
        (
            Outcome::CompletedWithWarning,
            format!("{summary} {details}"),
        )
    } else {
        let scope = match format {
            Format::Zip => text!("Структура и данные ZIP проверены."),
            Format::Tar => text!("Заголовки TAR проверены, данные прочитаны."),
        };
        (Outcome::Completed, format!("{scope} {summary}"))
    };
    report
        .progress
        .results
        .record(&source, None, outcome, reason);
    if matches!(outcome, Outcome::Completed | Outcome::Cancelled) {
        Ok(message)
    } else {
        Err(message)
    }
}

struct Report<'a> {
    source: &'a Location,
    progress: &'a mut Progress,
    passed: usize,
    damaged: usize,
    unverified: usize,
}

impl Report<'_> {
    fn check(&mut self, path: &Path, format: Format, password: Option<&str>) -> io::Result<()> {
        self.progress.check_cancelled()?;
        let mut archive = Extraction::open(path, password)?;
        match format {
            Format::Zip => self.check_zip(
                &mut archive,
                password.is_some_and(|secret| !secret.is_empty()),
            )?,
            Format::Tar => self.check_tar(&archive)?,
        }
        self.progress.check_cancelled()?;
        if !archive.source_unchanged() {
            return Err(invalid(crate::i18n::key!(
                "Архив изменился во время проверки"
            )));
        }
        Ok(())
    }

    fn check_zip(&mut self, archive: &mut Extraction<'_>, has_password: bool) -> io::Result<()> {
        let cancel = self.progress.cancellation();
        let (file, length) = archive.input();
        let mut records = Vec::new();
        let mut path_bytes = 0;
        let scanned = scan_zip(
            MetadataReader {
                file,
                length,
                position: 0,
                remaining: MAX_METADATA_BYTES,
                cancelled: &|| cancel.is_requested(),
                reading_payload: None,
            },
            true,
            |index, name, record| {
                self.progress.check_cancelled()?;
                match record {
                    Ok(record) => {
                        path_bytes += record.path.as_os_str().len();
                        if path_bytes > MAX_INDEX_BYTES {
                            return Err(too_large());
                        }
                        records.push((index, record));
                    }
                    Err(error) => self.member_result(index, name, Err(error))?,
                }
                Ok(())
            },
        );
        cancel.check()?;
        let protected_error = if has_password {
            archive
                .prepare_protected(
                    records
                        .iter()
                        .filter_map(|(_, record)| match record.payload {
                            Contents::Encrypted(member) => Some((member, record.member.size)),
                            _ => None,
                        }),
                    &cancel,
                )
                .err()
        } else {
            None
        };
        records.sort_unstable_by_key(|(_, record)| record.span.start);
        let mut records = records.into_iter().peekable();
        let mut previous_end = 0;
        while let Some((index, record)) = records.next() {
            self.progress.check_cancelled()?;
            let overlaps = record.span.start < previous_end
                || records
                    .peek()
                    .is_some_and(|(_, next)| next.span.start < record.span.end);
            previous_end = previous_end.max(record.span.end);
            if overlaps {
                self.member_result(
                    index,
                    &record.path.to_string_lossy(),
                    Err(invalid(crate::i18n::key!(
                        "Данные записей ZIP перекрываются"
                    ))),
                )?;
                continue;
            }
            let result = match record.payload {
                Contents::UnsupportedEncryption => Err(unsupported(crate::i18n::key!(
                    "Шифрование или сжатие ZIP не поддерживается"
                ))),
                Contents::UnsupportedCompression => Err(unsupported(crate::i18n::key!(
                    "Метод сжатия ZIP не поддерживается"
                ))),
                Contents::Encrypted(_) if !has_password => Err(unsupported(crate::i18n::key!(
                    "Укажите пароль для проверки ZIP"
                ))),
                Contents::Encrypted(_) if let Some(ref error) = protected_error => Err(
                    io::Error::new(io::ErrorKind::Unsupported, error.to_string()),
                ),
                _ => archive.check_payload(
                    record.payload,
                    record.member.size,
                    &record.path,
                    self.progress,
                ),
            };
            self.member_result(index, &record.path.to_string_lossy(), result)?;
        }
        scanned
    }

    fn check_tar(&mut self, archive: &Extraction<'_>) -> io::Result<()> {
        const BLOCK: u64 = 512;
        let cancel = self.progress.cancellation();
        let (file, length) = archive.input();
        if length < BLOCK {
            return Err(invalid(crate::i18n::key!("Некорректная длина TAR")));
        }
        let reading_payload = Cell::new(false);
        let cancelled = || cancel.is_requested();
        let mut tar = tar::Archive::new(MetadataReader {
            file,
            length,
            position: 0,
            remaining: MAX_METADATA_BYTES,
            cancelled: &cancelled,
            reading_payload: Some(&reading_payload),
        });
        for (index, entry) in tar.entries_with_seek()?.enumerate() {
            cancel.check()?;
            if index >= MAX_ENTRIES {
                return Err(too_large());
            }
            let mut entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.member_result(
                        index + 1,
                        text!("Заголовок TAR"),
                        Err(io::Error::new(error.kind(), error.to_string())),
                    )?;
                    return Err(error);
                }
            };
            let raw = entry.path_bytes().into_owned();
            let path = member_path(&raw);
            let global_pax = entry.header().entry_type().is_pax_global_extensions();
            let mut sparse_pax = false;
            let mut bad_pax = false;
            if let Some(extensions) = entry.pax_extensions()? {
                for extension in extensions {
                    match extension {
                        Ok(field) => sparse_pax |= field.key_bytes().starts_with(b"GNU.sparse."),
                        Err(_) => bad_pax = true,
                    }
                }
            }
            let result = path.and_then(|path| {
                if bad_pax {
                    return Err(invalid(crate::i18n::key!(
                        "Повреждён расширенный заголовок TAR"
                    )));
                }
                if sparse_pax {
                    return Err(unsupported(crate::i18n::key!(
                        "Разреженная запись TAR требует другой проверки"
                    )));
                }
                if global_pax {
                    // Reading the global PAX records already consumed this payload.
                    return Ok(());
                }
                reading_payload.set(true);
                let size = entry.size();
                let checked = copy_exact(&path, size, &mut entry, None, self.progress);
                reading_payload.set(false);
                checked
            });
            self.member_result(index + 1, &String::from_utf8_lossy(&raw), result)?;
        }
        check_tar_trailer(tar.into_inner(), self.source.path(), self.progress)
    }

    fn member_result(
        &mut self,
        index: usize,
        name: &str,
        result: io::Result<()>,
    ) -> io::Result<()> {
        self.progress.check_cancelled()?;
        let Err(error) = result else {
            self.passed += 1;
            return Ok(());
        };
        let outcome = if matches!(
            error.kind(),
            io::ErrorKind::Unsupported
                | io::ErrorKind::InvalidInput
                | io::ErrorKind::PermissionDenied
        ) {
            self.unverified += 1;
            Outcome::Skipped
        } else {
            self.damaged += 1;
            Outcome::Failed
        };
        let name = crate::tr_format!("Запись {index} · {name}", "Entry {index} · {name}");
        self.progress
            .results
            .diagnostic(self.source, &name, outcome, &error.to_string());
        Ok(())
    }
}

pub(super) fn check_tar_trailer(
    input: MetadataReader<'_>,
    path: &Path,
    progress: &mut Progress,
) -> io::Result<()> {
    const BLOCK: u64 = 512;
    // The iterator consumed the first zero header. TAR ends with a second
    // zero block and may have additional zero padding.
    let remaining = input.length - input.position;
    if remaining < BLOCK || !input.length.is_multiple_of(BLOCK) {
        return Err(invalid(crate::i18n::key!(
            "Отсутствуют завершающие блоки TAR"
        )));
    }
    let reading_payload = Cell::new(true);
    let trailer = MetadataReader {
        reading_payload: Some(&reading_payload),
        ..input
    };
    let read = progress.read_stream(path, remaining, trailer, |reader| {
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                return Ok(());
            }
            if buffer[..count].iter().any(|&byte| byte != 0) {
                return Err(invalid(crate::i18n::key!(
                    "Неожиданные данные после конца TAR"
                )));
            }
        }
    })?;
    if read != remaining {
        return Err(invalid(crate::i18n::key!(
            "Отсутствуют завершающие блоки TAR"
        )));
    }
    Ok(())
}

fn unsupported(message: crate::i18n::MessageKey) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, text(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::Cancellation;
    use std::{
        fs::{self, File},
        io::{Read, Write},
    };

    #[test]
    fn zip_reports_each_damaged_record_even_when_member_names_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entries.zip");
        let mut zip = rawzip::ZipArchiveWriter::new(Vec::new());
        zip.new_dir("empty/").create().unwrap();
        for (name, body) in [
            ("same.txt", b"one"),
            ("same.txt", b"two"),
            ("later.txt", b"end"),
        ] {
            let (mut entry, config) = zip.new_file(name).start().unwrap();
            let mut body_writer = config.wrap(&mut entry);
            body_writer.write_all(body).unwrap();
            let (_, descriptor) = body_writer.finish().unwrap();
            entry.finish(descriptor).unwrap();
        }
        let mut bytes = zip.finish().unwrap();
        fs::write(&path, &bytes).unwrap();
        let mut progress = Progress::default();
        let good = verify_archive(&path, None, &mut progress).unwrap();
        assert!(good.contains("Verified entries: 4"), "{good}");

        let (first_payload, last_header) = {
            let zip = rawzip::ZipArchive::from_slice(&bytes).unwrap();
            let mut entries = zip.entries();
            entries.next_entry().unwrap().unwrap();
            let first = entries.next_entry().unwrap().unwrap();
            let offset = zip
                .get_entry(first.wayfinder())
                .unwrap()
                .compressed_data_range()
                .0;
            entries.next_entry().unwrap().unwrap();
            let last = entries.next_entry().unwrap().unwrap();
            (offset as usize, last.local_header_offset() as usize)
        };
        bytes[first_payload] ^= 1;
        bytes[last_header + 30] = b'X'; // First filename byte of the local header.
        fs::write(&path, &bytes).unwrap();
        let error = verify_archive(&path, None, &mut progress).unwrap_err();
        assert!(
            error.contains("Verified entries: 2. Entry errors: 2"),
            "{error}"
        );
        let [root, header_error, crc_error] = progress.results.items.as_slice() else {
            panic!("expected the archive result and two distinct entry diagnostics");
        };
        assert_eq!(root.outcome, Outcome::Failed);
        assert!(root.descendants_enumerated);
        assert_eq!(header_error.source, "Entry 4 · later.txt");
        assert_eq!(
            header_error.reason.as_deref(),
            Some("ZIP local header disagrees with the directory")
        );
        assert_eq!(crc_error.source, "Entry 2 · same.txt");
        assert_eq!(crc_error.reason.as_deref(), Some("ZIP CRC does not match"));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn zip_checks_encrypted_entries_through_authentication_and_keeps_plain_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.zip");
        let password = "disposable archive integrity fixture";
        let mut zip = zip::ZipWriter::new_stream(Vec::new());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("plain.txt", options).unwrap();
        zip.write_all(b"plain contents").unwrap();
        zip.start_file(
            "secret.txt",
            options.with_aes_encryption(zip::AesMode::Aes256, password),
        )
        .unwrap();
        zip.write_all(b"encrypted contents").unwrap();
        let mut bytes = zip.finish().unwrap().into_inner();
        fs::write(&path, &bytes).unwrap();
        let mut progress = Progress::default();
        let partial = verify_archive(&path, None, &mut progress).unwrap_err();
        assert!(
            partial.contains("Verified entries: 1. Entry errors: 0. Unverified: 1"),
            "{partial}"
        );
        assert_eq!(
            progress.results.items[0].outcome,
            Outcome::CompletedWithWarning
        );
        assert_eq!(progress.results.items[1].outcome, Outcome::Skipped);
        assert_eq!(progress.results.items[1].source, "Entry 2 · secret.txt");
        let good = verify_archive(&path, Some(password), &mut progress).unwrap();
        assert!(good.contains("Verified entries: 2"), "{good}");

        let last_auth_byte = {
            let zip = rawzip::ZipArchive::from_slice(&bytes).unwrap();
            let mut entries = zip.entries();
            entries.next_entry().unwrap().unwrap();
            let secret = entries.next_entry().unwrap().unwrap();
            (zip.get_entry(secret.wayfinder())
                .unwrap()
                .compressed_data_range()
                .1
                - 1) as usize
        };
        bytes[last_auth_byte] ^= 1;
        fs::write(&path, bytes).unwrap();
        let error = verify_archive(&path, Some(password), &mut progress).unwrap_err();
        assert!(
            error.contains("Verified entries: 1. Entry errors: 1"),
            "{error}"
        );
        let record = progress
            .results
            .finish("Check archive".into(), 0, &Err(error))
            .unwrap();
        assert_eq!(record.items[1].source, "Entry 2 · secret.txt");
        assert_eq!(record.items[1].outcome, Outcome::Failed);
        assert!(!serde_json::to_string(&record).unwrap().contains(password));
    }

    #[test]
    fn tar_reads_large_payloads_and_stops_between_read_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.tar");
        let size = MAX_METADATA_BYTES as u64 + 512;
        let mut tar = tar::Builder::new(File::create(&path).unwrap());
        let mut header = tar::Header::new_ustar();
        header.set_size(0);
        tar.append_data(&mut header, "empty", io::empty()).unwrap();
        header.set_size(size);
        tar.append_data(&mut header, "large.bin", io::repeat(0x5a).take(size))
            .unwrap();
        tar.finish().unwrap();
        drop(tar);

        let mut progress = Progress::default();
        let good = verify_archive(&path, None, &mut progress).unwrap();
        assert!(good.contains("Verified entries: 2"), "{good}");

        let mut checkpoints = 0;
        let cancel = Cancellation::at_checkpoint(move || {
            checkpoints += 1;
            checkpoints == 20
        });
        let mut progress = Progress::with_cancellation(cancel);
        let stopped = verify_archive(&path, None, &mut progress).unwrap();
        assert!(
            stopped.contains("Archive check stopped. Verified entries: 1"),
            "{stopped}"
        );
        assert!(progress.results.cancelled);
        assert_eq!(progress.results.items[0].outcome, Outcome::Cancelled);
        assert_eq!(fs::metadata(&path).unwrap().len(), size + 4 * 512);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn tar_exposes_a_bad_header_and_requires_the_ending_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tar");
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_ustar();
        header.set_size(0);
        tar.append_data(&mut header, "first", io::empty()).unwrap();
        tar.append_data(&mut header, "second", io::empty()).unwrap();
        let mut bytes = tar.into_inner().unwrap();
        fs::write(&path, &bytes[..bytes.len() - 512]).unwrap();
        let mut progress = Progress::default();
        let ending = verify_archive(&path, None, &mut progress).unwrap_err();
        assert!(ending.contains("TAR end blocks are missing"), "{ending}");
        bytes[512] ^= 1;
        fs::write(&path, bytes).unwrap();
        let damaged = verify_archive(&path, None, &mut progress).unwrap_err();
        assert!(
            damaged.contains("Verified entries: 1. Entry errors: 1"),
            "{damaged}"
        );
        assert_eq!(progress.results.items[1].source, "Entry 2 · TAR header");
        assert_eq!(progress.results.items[1].outcome, Outcome::Failed);
    }
}
