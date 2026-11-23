use super::extraction::{copy_exact, Extraction};
use super::{
    member_path, scan_zip, too_large, ArchiveIssue, Contents, Format, MetadataReader, MAX_ENTRIES,
    MAX_INDEX_BYTES, MAX_METADATA_BYTES,
};
use crate::{
    file_error::{AccessAction, AccessError, ArchiveError},
    operations::{Progress, TransferError},
    result_history::Outcome,
    vfs::Location,
};
use std::{cell::Cell, io, path::Path, sync::Arc};

#[derive(Debug)]
pub(crate) struct ArchiveVerification {
    pub(crate) format: Format,
    pub(crate) passed: usize,
    pub(crate) damaged: usize,
    pub(crate) unverified: usize,
    pub(crate) interruption: Option<TransferError>,
    #[cfg(target_os = "macos")]
    pub(crate) first_issue: Option<MemberIssue>,
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
pub(crate) struct MemberIssue {
    pub(crate) index: usize,
    pub(crate) name: Option<String>,
    pub(crate) source: Arc<io::Error>,
}

pub(crate) fn verify_archive(
    path: &Path,
    password: Option<&str>,
    progress: &mut Progress,
) -> Result<ArchiveVerification, TransferError> {
    let source = Location::Local(path.to_owned());
    progress.results.plan([(source.clone(), None)]);
    progress.results.begin(&source);
    progress.item(path);
    let Some(format) = Format::from_path(path) else {
        let error = ArchiveError::UnsupportedFormat(source.clone()).into();
        progress
            .results
            .record_with_error(&source, Outcome::Failed, Some(&error));
        return Err(error);
    };
    let mut report = Report {
        source: &source,
        progress,
        verification: ArchiveVerification {
            format,
            passed: 0,
            damaged: 0,
            unverified: 0,
            interruption: None,
            #[cfg(target_os = "macos")]
            first_issue: None,
        },
    };
    let result = report.check(path, format, password);
    report.verification.interruption = if report.progress.cancellation().is_requested() {
        Some(TransferError::Cancelled)
    } else {
        result.err()
    };
    let findings = &report.verification;
    let outcome = match &findings.interruption {
        Some(TransferError::Cancelled) => {
            report.progress.results.cancelled = true;
            Outcome::Cancelled
        }
        Some(_) => Outcome::Failed,
        None => {
            report.progress.results.finish_diagnostics(&source);
            if findings.damaged != 0 {
                Outcome::Failed
            } else if findings.unverified != 0 {
                Outcome::CompletedWithWarning
            } else {
                Outcome::Completed
            }
        }
    };
    let error = findings
        .interruption
        .as_ref()
        .filter(|_| outcome != Outcome::Cancelled);
    report
        .progress
        .results
        .record_with_error(&source, outcome, error);
    Ok(report.verification)
}

struct Report<'a> {
    source: &'a Location,
    progress: &'a mut Progress,
    verification: ArchiveVerification,
}

impl Report<'_> {
    fn check(
        &mut self,
        path: &Path,
        format: Format,
        password: Option<&str>,
    ) -> Result<(), TransferError> {
        self.progress.check_cancelled()?;
        let mut archive = Extraction::open(path, password)?;
        match format {
            Format::Zip => self.check_zip(
                &mut archive,
                password.is_some_and(|secret| !secret.is_empty()),
            ),
            Format::Tar => self.check_tar(&archive),
        }
        .map_err(|source| ArchiveError::Verify {
            archive: self.source.clone(),
            source,
        })?;
        self.progress.check_cancelled()?;
        if !archive
            .source_matches()
            .map_err(|source| AccessError::new(self.source, AccessAction::ReadMetadata, source))?
        {
            return Err(ArchiveError::ChangedDuringVerification(self.source.clone()).into());
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
                    Err(error) => self.member_result(index, Some(name), Err(error))?,
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
                .map(Arc::new)
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
            let name = Some(record.path.to_string_lossy());
            let name = name.as_deref();
            if overlaps {
                self.member_result(
                    index,
                    name,
                    Err(ArchiveIssue::OverlappingZipEntries.io_error()),
                )?;
                continue;
            }
            let result = match record.payload {
                Contents::UnsupportedEncryption => {
                    Err(ArchiveIssue::UnsupportedZipEncryptionOrCompression.io_error())
                }
                Contents::UnsupportedCompression => {
                    Err(ArchiveIssue::UnsupportedZipCompression.io_error())
                }
                Contents::Encrypted(_) if !has_password => {
                    Err(ArchiveIssue::ZipCheckPasswordRequired.io_error())
                }
                Contents::Encrypted(_) if let Some(ref error) = protected_error => {
                    self.member_failure(index, name, Outcome::Skipped, error.clone())?;
                    continue;
                }
                _ => archive.check_payload(
                    record.payload,
                    record.member.size,
                    &record.path,
                    self.progress,
                ),
            };
            self.member_result(index, name, result)?;
        }
        scanned
    }

    fn check_tar(&mut self, archive: &Extraction<'_>) -> io::Result<()> {
        const BLOCK: u64 = 512;
        let cancel = self.progress.cancellation();
        let (file, length) = archive.input();
        if length < BLOCK {
            return Err(ArchiveIssue::InvalidTarLength.io_error());
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
                    self.record_member_failure(index + 1, None, member_outcome(&error), &error)?;
                    return Err(error);
                }
            };
            let raw = entry.path_bytes().into_owned();
            let path = member_path(&raw);
            let global_pax = entry.header().entry_type().is_pax_global_extensions();
            let mut sparse_pax = false;
            let mut bad_pax = None;
            if let Some(extensions) = entry.pax_extensions()? {
                for extension in extensions {
                    match extension {
                        Ok(field) => sparse_pax |= field.key_bytes().starts_with(b"GNU.sparse."),
                        Err(error) => {
                            bad_pax = Some(error);
                            break;
                        }
                    }
                }
            }
            let result = path.and_then(|path| {
                if let Some(error) = bad_pax {
                    return Err(ArchiveIssue::InvalidTarExtendedHeader.with_source(error));
                }
                if sparse_pax {
                    return Err(ArchiveIssue::UnverifiableSparseTar.io_error());
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
            self.member_result(index + 1, Some(&String::from_utf8_lossy(&raw)), result)?;
        }
        check_tar_trailer(tar.into_inner(), self.source.path(), self.progress)
    }

    fn member_result(
        &mut self,
        index: usize,
        name: Option<&str>,
        result: io::Result<()>,
    ) -> io::Result<()> {
        let Err(error) = result else {
            self.progress.check_cancelled()?;
            self.verification.passed += 1;
            return Ok(());
        };
        self.member_failure(index, name, member_outcome(&error), Arc::new(error))
    }

    fn member_failure(
        &mut self,
        index: usize,
        name: Option<&str>,
        outcome: Outcome,
        error: Arc<io::Error>,
    ) -> io::Result<()> {
        self.record_member_failure(index, name, outcome, &error)?;
        #[cfg(target_os = "macos")]
        if self.verification.first_issue.is_none() {
            self.verification.first_issue = Some(MemberIssue {
                index,
                name: name.map(str::to_owned),
                source: error,
            });
        }
        Ok(())
    }

    fn record_member_failure(
        &mut self,
        index: usize,
        name: Option<&str>,
        outcome: Outcome,
        error: &io::Error,
    ) -> io::Result<()> {
        self.progress.check_cancelled()?;
        if outcome == Outcome::Skipped {
            self.verification.unverified += 1;
        } else {
            self.verification.damaged += 1;
        }
        self.progress
            .results
            .archive_diagnostic(self.source, index, name, outcome, error);
        Ok(())
    }
}

fn member_outcome(error: &io::Error) -> Outcome {
    match error.kind() {
        io::ErrorKind::Unsupported
        | io::ErrorKind::InvalidInput
        | io::ErrorKind::PermissionDenied => Outcome::Skipped,
        _ => Outcome::Failed,
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
        return Err(ArchiveIssue::MissingTarTrailer.io_error());
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
                return Err(ArchiveIssue::UnexpectedTarTrailerData.io_error());
            }
        }
    })?;
    if read != remaining {
        return Err(ArchiveIssue::MissingTarTrailer.io_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_error::{AccessCause, Failure};
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
        assert_eq!((good.passed, good.damaged, good.unverified), (4, 0, 0));
        assert!(good.interruption.is_none());

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
        let damaged = verify_archive(&path, None, &mut progress).unwrap();
        assert_eq!(
            (damaged.passed, damaged.damaged, damaged.unverified),
            (2, 2, 0)
        );
        assert!(damaged.interruption.is_none());
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
        let partial = verify_archive(&path, None, &mut progress).unwrap();
        assert_eq!(
            (partial.passed, partial.damaged, partial.unverified),
            (1, 0, 1)
        );
        assert!(partial.interruption.is_none());
        assert_eq!(
            progress.results.items[0].outcome,
            Outcome::CompletedWithWarning
        );
        assert_eq!(progress.results.items[1].outcome, Outcome::Skipped);
        assert_eq!(progress.results.items[1].source, "Entry 2 · secret.txt");
        let good = verify_archive(&path, Some(password), &mut progress).unwrap();
        assert_eq!((good.passed, good.damaged, good.unverified), (2, 0, 0));
        assert!(good.interruption.is_none());

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
        let damaged = verify_archive(&path, Some(password), &mut progress).unwrap();
        assert_eq!(
            (damaged.passed, damaged.damaged, damaged.unverified),
            (1, 1, 0)
        );
        assert!(damaged.interruption.is_none());
        let record = progress
            .results
            .finish("Check archive".into(), 0, &Err("Damaged entry".into()))
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
        assert_eq!((good.passed, good.damaged, good.unverified), (2, 0, 0));
        assert!(good.interruption.is_none());

        let mut checkpoints = 0;
        let cancel = Cancellation::at_checkpoint(move || {
            checkpoints += 1;
            checkpoints == 20
        });
        let mut progress = Progress::with_cancellation(cancel);
        let stopped = verify_archive(&path, None, &mut progress).unwrap();
        assert_eq!(
            (stopped.passed, stopped.damaged, stopped.unverified),
            (1, 0, 0)
        );
        assert!(matches!(
            stopped.interruption,
            Some(TransferError::Cancelled)
        ));
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
        let ending = verify_archive(&path, None, &mut progress).unwrap();
        let Some(TransferError::Failed(Failure::Archive(error))) = ending.interruption else {
            panic!("expected a typed archive verification error");
        };
        let ArchiveError::Verify { archive, source } = *error else {
            panic!("expected a format error: {error}");
        };
        assert_eq!(archive.path(), path);
        assert_eq!(source.kind(), io::ErrorKind::InvalidData);
        assert_eq!(source.to_string(), "TAR end blocks are missing");
        assert!(!progress.results.items[0].descendants_enumerated);
        bytes[512] ^= 1;
        fs::write(&path, bytes).unwrap();
        let damaged = verify_archive(&path, None, &mut progress).unwrap();
        assert_eq!(
            (damaged.passed, damaged.damaged, damaged.unverified),
            (1, 1, 0)
        );
        assert!(matches!(
            damaged.interruption,
            Some(TransferError::Failed(Failure::Archive(_)))
        ));
        assert_eq!(progress.results.items[1].source, "Entry 2 · TAR header");
        assert_eq!(progress.results.items[1].outcome, Outcome::Failed);
    }

    #[test]
    fn an_archive_open_failure_retains_the_path_and_os_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.zip");
        let mut progress = Progress::default();
        let report = verify_archive(&path, None, &mut progress).unwrap();
        let Some(TransferError::Failed(Failure::Access(error))) = report.interruption else {
            panic!("expected a typed archive access error");
        };
        assert_eq!(error.location.path(), path);
        assert_eq!(error.action, AccessAction::OpenFile);
        assert!(
            matches!(error.cause, AccessCause::Local(source) if source.kind() == io::ErrorKind::NotFound)
        );
        let row = &progress.results.items[0];
        assert_eq!(row.outcome, Outcome::Failed);
        assert_eq!(row.source, path.display().to_string());
        assert!(!row.descendants_enumerated);
    }
}
