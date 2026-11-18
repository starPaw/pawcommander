use super::{invalid, too_large, MetadataReader, MAX_ENTRIES, MAX_METADATA_BYTES};
use crate::{i18n::text, operations::Cancellation};
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
    entries: BTreeMap<usize, OwnedZipFileEntry>,
}

impl ProtectedZip {
    pub(super) fn read(
        file: &File,
        length: u64,
        selection: impl Iterator<Item = (Member, u64)>,
        password: Option<&str>,
        cancel: &Cancellation,
    ) -> io::Result<Self> {
        let mut wanted: BTreeMap<_, _> = selection
            .map(|(member, size)| (member.index, (member, size)))
            .collect();
        let mut archive = Self::default();
        let (Some(&(first, _)), Some((&last, _))) =
            (wanted.values().next(), wanted.last_key_value())
        else {
            return Ok(archive);
        };
        let password = password.ok_or_else(password_required)?;
        let metadata = MetadataReader {
            file,
            length,
            position: 0,
            remaining: MAX_METADATA_BYTES,
            cancelled: &|| cancel.is_requested(),
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
                if item.header_start() != expected.header
                    || item.compressed_size() != expected.length
                    || item.size() != size
                    || item.crc32() != expected.crc
                    || !item.encrypted()
                {
                    return Err(invalid("Недопустимая запись архива"));
                }
                if !matches!(
                    item.compression(),
                    CompressionMethod::Stored | CompressionMethod::Deflated
                ) {
                    return Err(invalid("Метод сжатия ZIP не поддерживается"));
                }
                archive.entries.insert(index, item.into());
            }
        }
        if !wanted.is_empty() {
            return Err(invalid("Недопустимая запись архива"));
        }
        // Reject an ordinary wrong password before asking about destination
        // conflicts. Every entry is still checked through EOF at extraction.
        let mut input = EntryReader { file, cancel };
        drop(archive.open(first, &mut input, password)?);
        Ok(archive)
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
            .ok_or_else(|| invalid("Недопустимая запись архива"))?;
        let options = ZipReadOptions::new().password(Some(password.as_bytes()));
        let entry = entry
            .as_ref()
            .with_reader(input, options)
            .map_err(zip_error)?;
        if entry.data_start() != Some(member.data) {
            return Err(invalid("Недопустимая запись архива"));
        }
        Ok(entry)
    }
}

pub(super) fn password_required() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        text("Укажите пароль для распаковки ZIP"),
    )
}

pub(super) fn zip_error(error: ZipError) -> io::Error {
    match error {
        ZipError::Io(error) => error,
        ZipError::InvalidPassword => {
            io::Error::new(io::ErrorKind::InvalidInput, text("Неверный пароль ZIP"))
        }
        ZipError::UnsupportedArchive(_) | ZipError::CompressionMethodNotSupported(_) => {
            invalid("Шифрование или сжатие ZIP не поддерживается")
        }
        _ => invalid("Недопустимая запись архива"),
    }
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
