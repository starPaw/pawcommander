use super::{too_large, ArchiveIssue, MetadataReader, MAX_ENTRIES, MAX_METADATA_BYTES};
use crate::operations::Cancellation;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
};
use zip::{
    read::{Config, OwnedZipFileEntry, ZipFile},
    result::ZipError,
    unstable::read::ZipIterable,
    CompressionMethod, ZipReadOptions,
};

#[derive(Clone, Copy)]
pub(super) struct Member {
    pub index: usize,
    pub header: u64,
    pub data: u64,
    pub length: u64,
    pub crc: u32,
}

#[derive(Default)]
pub(super) struct ProtectedZip {
    entries: BTreeMap<usize, Result<OwnedZipFileEntry, EntryProblem>>,
}

enum EntryProblem {
    Invalid,
    UnsupportedCompression,
}

impl EntryProblem {
    fn io_error(&self) -> io::Error {
        match self {
            Self::Invalid => ArchiveIssue::InvalidEntry.io_error(),
            Self::UnsupportedCompression => ArchiveIssue::UnsupportedZipCompression.io_error(),
        }
    }
}

impl ProtectedZip {
    pub(super) fn read(
        file: &File,
        length: u64,
        selection: impl Iterator<Item = (Member, u64)>,
        cancel: &Cancellation,
    ) -> io::Result<Self> {
        let mut wanted: BTreeMap<_, _> = selection
            .map(|(member, size)| (member.index, (member, size)))
            .collect();
        let mut archive = Self::default();
        let Some((&last, _)) = wanted.last_key_value() else {
            return Ok(archive);
        };
        let metadata = MetadataReader {
            file,
            length,
            position: 0,
            remaining: MAX_METADATA_BYTES,
            cancelled: &|| cancel.is_requested(),
            reading_payload: None,
        };
        let mut zip = ZipIterable::try_new(metadata, Config::default()).map_err(zip_error)?;
        if zip.len() > MAX_ENTRIES {
            return Err(too_large());
        }
        for (index, item) in zip.files().map_err(zip_error)?.take(last + 1).enumerate() {
            cancel.check()?;
            let item = item.map_err(zip_error)?;
            if let Some((expected, size)) = wanted.remove(&index) {
                // Paths come only from the bounded raw ZIP index. Match the
                // decoder's record to the exact encrypted bytes it selected.
                let entry = if item.header_start() != expected.header
                    || item.compressed_size() != expected.length
                    || item.size() != size
                    || item.crc32() != expected.crc
                    || !item.encrypted()
                {
                    Err(EntryProblem::Invalid)
                } else if !matches!(
                    item.compression(),
                    CompressionMethod::Stored | CompressionMethod::Deflated
                ) {
                    Err(EntryProblem::UnsupportedCompression)
                } else {
                    Ok(item.into())
                };
                archive.entries.insert(index, entry);
            }
        }
        if !wanted.is_empty() {
            return Err(ArchiveIssue::InvalidEntry.io_error());
        }
        Ok(archive)
    }

    pub(super) fn validate(&self) -> io::Result<()> {
        self.entries
            .values()
            .try_for_each(|entry| entry.as_ref().map(|_| ()).map_err(EntryProblem::io_error))
    }

    pub(super) fn open<'a, R: Read + Seek>(
        &'a self,
        member: Member,
        input: &'a mut R,
        password: &str,
    ) -> io::Result<ZipFile<'a, R>> {
        let entry = self
            .entries
            .get(&member.index)
            .ok_or_else(|| ArchiveIssue::InvalidEntry.io_error())?
            .as_ref()
            .map_err(EntryProblem::io_error)?;
        let options = ZipReadOptions::new().password(Some(password.as_bytes()));
        let entry = entry
            .as_ref()
            .with_reader(input, options)
            .map_err(zip_error)?;
        if entry.data_start() != Some(member.data) {
            return Err(ArchiveIssue::InvalidEntry.io_error());
        }
        Ok(entry)
    }
}

pub(super) fn password_required() -> io::Error {
    ArchiveIssue::ZipExtractionPasswordRequired.io_error()
}

pub(super) fn zip_error(error: ZipError) -> io::Error {
    let issue = match error {
        ZipError::Io(error) => return error,
        ZipError::InvalidPassword => ArchiveIssue::InvalidZipPassword,
        ZipError::UnsupportedArchive(_) | ZipError::CompressionMethodNotSupported(_) => {
            ArchiveIssue::UnsupportedZipEncryptionOrCompression
        }
        _ => ArchiveIssue::InvalidEntry,
    };
    issue.with_source(error)
}

pub(super) struct EntryReader<'a> {
    pub file: &'a File,
    pub cancel: &'a Cancellation,
}

impl Read for EntryReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.cancel.check()?;
        self.file.read(buffer)
    }
}

impl Seek for EntryReader<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.cancel.check()?;
        self.file.seek(position)
    }
}
