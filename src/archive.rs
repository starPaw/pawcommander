//! Archive formats and directory indexes. The index uses virtual member paths.
mod extraction;
mod integrity;
mod protected_zip;
mod writer;

pub(crate) use extraction::{Extraction, SelectedTree};
pub(crate) use integrity::verify_archive;
pub(crate) use writer::{ArchiveWriter, Attributes, CreationLimits};

use crate::{
    fs_ops::ListingMetadata,
    i18n::text,
    vfs::{FileEntry, Location},
};
use chrono::TimeZone;
use std::{
    borrow::Cow,
    cell::Cell,
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    ops::Range,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_ENTRIES: usize = 50_000;
const MAX_INDEX_BYTES: usize = 12 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 32 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_DEPTH: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Zip,
    Tar,
}

impl Format {
    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Tar => "tar",
        }
    }

    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?;
        if extension.eq_ignore_ascii_case("zip") {
            Some(Self::Zip)
        } else if extension.eq_ignore_ascii_case("tar") {
            Some(Self::Tar)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Member {
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    modified: Option<SystemTime>,
    mode: Option<u16>,
    contents: Contents,
}

#[derive(Clone, Copy, Default)]
enum Contents {
    Directory,
    Tar {
        offset: u64,
    },
    Zip {
        offset: u64,
        length: u64,
        crc: u32,
        deflate: bool,
    },
    Encrypted(protected_zip::Member),
    UnsupportedEncryption,
    UnsupportedCompression,
    #[default]
    UnsupportedType,
}

#[derive(Default)]
pub(crate) struct Index {
    folders: BTreeMap<PathBuf, BTreeMap<OsString, Member>>,
    nodes: usize,
    path_bytes: usize,
}

impl Index {
    pub(crate) fn load(file: &Path, cancelled: &impl Fn() -> bool) -> Result<Self, String> {
        let format =
            Format::from_path(file).ok_or_else(|| text("Поддерживаются архивы ZIP и TAR"))?;
        if cancelled() {
            return Err(cancelled_read().to_string());
        }
        open_regular(file)
            .and_then(|(file, metadata)| Self::read(&file, metadata.len(), format, cancelled))
            .map_err(|error| {
                crate::tr_format!(
                    "Не удалось прочитать архив «{}»: {error}",
                    "Could not read archive “{}”: {error}",
                    file.display()
                )
            })
    }

    fn read(
        file: &File,
        length: u64,
        format: Format,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Self> {
        let mut index = Self::default();
        index.folders.insert(PathBuf::new(), BTreeMap::new());
        let input = MetadataReader {
            file,
            length,
            position: 0,
            remaining: MAX_METADATA_BYTES,
            cancelled,
            reading_payload: None,
        };
        match format {
            Format::Zip => index.read_zip(input)?,
            Format::Tar => index.read_tar(input)?,
        }
        Ok(index)
    }

    pub(crate) fn list(
        &self,
        location: &Location,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Vec<FileEntry>, String> {
        let members = self
            .folders
            .get(location.path())
            .ok_or_else(|| text("Папка отсутствует в архиве").to_owned())?;
        Ok(members
            .iter()
            .take_while(|_| !cancelled())
            .map(|(name, member)| FileEntry {
                path: location.join(name),
                name: name.to_string_lossy().into_owned(),
                is_dir: member.is_dir,
                is_symlink: member.is_symlink,
                size: member.size,
                modified: member.modified,
                metadata: ListingMetadata {
                    mode: member.mode,
                    ..ListingMetadata::default()
                },
            })
            .collect())
    }

    fn add(&mut self, path: &Path, member: Member) -> io::Result<()> {
        let Some(parent) = path.parent() else {
            return if member.is_dir {
                Ok(())
            } else {
                Err(invalid_path())
            };
        };
        if !self.folders.contains_key(parent) {
            self.add(
                parent,
                Member {
                    is_dir: true,
                    contents: Contents::Directory,
                    ..Member::default()
                },
            )?;
        }
        let folder = self
            .folders
            .get_mut(parent)
            .expect("indexed archive parent");
        let name = path.file_name().ok_or_else(invalid_path)?;
        if let Some(existing) = folder.get_mut(name) {
            if existing.is_dir == member.is_dir {
                // Appended TAR entries and duplicate ZIP names describe the
                // same virtual path. Show the last recorded metadata.
                *existing = member;
                return Ok(());
            }
            return Err(invalid("Путь в архиве занят и папкой, и файлом"));
        }
        self.nodes += 1;
        self.path_bytes += path.as_os_str().len();
        if self.nodes > MAX_ENTRIES || self.path_bytes > MAX_INDEX_BYTES {
            return Err(too_large());
        }
        folder.insert(name.to_os_string(), member);
        if member.is_dir {
            self.folders.entry(path.to_owned()).or_default();
        }
        Ok(())
    }

    fn read_zip(&mut self, input: MetadataReader<'_>) -> io::Result<()> {
        scan_zip(input, false, |_, _, entry| {
            let entry = entry?;
            self.add(&entry.path, entry.member)
        })
    }

    fn read_tar(&mut self, input: MetadataReader<'_>) -> io::Result<()> {
        let cancelled = input.cancelled;
        if input.length < 512 {
            return Err(invalid("Недопустимая запись архива"));
        }
        let mut archive = tar::Archive::new(input);
        for (index, entry) in archive.entries_with_seek()?.enumerate() {
            if cancelled() {
                return Err(cancelled_read());
            }
            if index >= MAX_ENTRIES {
                return Err(too_large());
            }
            let mut entry = entry?;
            let kind = entry.header().entry_type();
            if kind.is_pax_global_extensions() {
                continue;
            }
            let sparse = entry.pax_extensions()?.is_some_and(|mut extensions| {
                extensions.any(|extension| {
                    extension.is_ok_and(|field| field.key_bytes().starts_with(b"GNU.sparse."))
                })
            });
            let header = entry.header();
            let name = entry.path_bytes();
            let path = member_path(&name)?;
            let is_dir = kind.is_dir()
                || (kind.is_file() && header.as_ustar().is_none() && name.ends_with(b"/"));
            let contents = if is_dir {
                Contents::Directory
            } else if !sparse && (kind.is_file() || kind.is_contiguous()) {
                Contents::Tar {
                    offset: entry.raw_file_position(),
                }
            } else {
                Contents::UnsupportedType
            };
            self.add(
                &path,
                Member {
                    is_dir,
                    is_symlink: kind.is_symlink(),
                    size: entry.size(),
                    modified: header
                        .mtime()
                        .ok()
                        .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs))),
                    mode: header.mode().ok().map(|mode| (mode & 0o7777) as u16),
                    contents,
                },
            )?;
        }
        Ok(())
    }
}

struct ZipRecord {
    path: PathBuf,
    member: Member,
    payload: Contents,
    span: Range<u64>,
}

fn scan_zip(
    input: MetadataReader<'_>,
    check_headers: bool,
    mut visit: impl FnMut(usize, &str, io::Result<ZipRecord>) -> io::Result<()>,
) -> io::Result<()> {
    let cancelled = input.cancelled;
    let mut buffer = vec![0; 3 * usize::from(u16::MAX)];
    let mut headers = check_headers.then(|| vec![0; 2 * usize::from(u16::MAX)]);
    let archive =
        rawzip::ZipArchive::from_seekable(input, &mut buffer).map_err(io::Error::other)?;
    let mut entries = archive.entries(&mut buffer);
    let mut count = 0;
    while let Some(entry) = entries.next_entry().map_err(io::Error::other)? {
        if cancelled() {
            return Err(cancelled_read());
        }
        count += 1;
        if count > MAX_ENTRIES {
            return Err(too_large());
        }
        let name = zip_name(&entry);
        let shown = match &name {
            Ok(name) => Cow::Borrowed(name.as_ref()),
            Err(_) => String::from_utf8_lossy(entry.file_path().as_bytes()),
        };
        let record = (|| {
            let name = name.as_ref().map_err(|_| invalid_path())?;
            let path = if name.contains('\\') {
                member_path(name.replace('\\', "/").as_bytes())?
            } else {
                member_path(name.as_bytes())?
            };
            if check_headers && path.as_os_str().is_empty() && !name.ends_with(['/', '\\']) {
                return Err(invalid_path());
            }
            let mode = entry.mode();
            let file_type = match entry.version_made_by().creator_system() {
                rawzip::CreatorSystem::UNIX | rawzip::CreatorSystem::MACOS => {
                    (entry.external_attributes() >> 16) & 0o170000
                }
                _ => 0,
            };
            let is_symlink = file_type == 0o120000;
            let is_dir =
                !is_symlink && (mode.value() & 0o170000 == 0o040000 || name.ends_with(['/', '\\']));
            let directory = archive.directory_offset();
            if entry.local_header_offset() >= directory {
                return Err(invalid("Недопустимая запись архива"));
            }
            let local = archive
                .get_entry(entry.wayfinder())
                .map_err(io::Error::other)?;
            let (offset, end) = local.compressed_data_range();
            if end > directory {
                return Err(invalid("Недопустимая запись архива"));
            }
            let mut record_end = end;
            if let Some(buffer) = &mut headers {
                let local_header = local.local_header(buffer).map_err(io::Error::other)?;
                if local_header.file_path().as_bytes() != entry.file_path().as_bytes()
                    || local_header.flags() != entry.flags()
                    || local_header.compression_method() != entry.compression_method()
                {
                    return Err(invalid("Локальный заголовок ZIP отличается от каталога"));
                }
                if entry.flags().has_data_descriptor() {
                    record_end = check_zip_descriptor(
                        archive.get_ref(),
                        &entry,
                        &local_header,
                        end,
                        directory,
                    )?;
                } else if (
                    local_header.crc32(),
                    local_header.compressed_size_hint(),
                    local_header.uncompressed_size_hint(),
                ) != (
                    entry.crc32(),
                    entry.compressed_size_hint(),
                    entry.uncompressed_size_hint(),
                ) {
                    return Err(invalid(
                        "Размер или контрольная сумма ZIP отличаются в заголовках",
                    ));
                }
            }
            let payload = zip_payload(&entry, count - 1, offset, end - offset);
            let contents = if !matches!(file_type, 0 | 0o100000 | 0o040000) {
                Contents::UnsupportedType
            } else if !is_dir {
                payload
            } else if entry.flags().is_encrypted()
                || entry.flags().has_strong_encryption()
                || entry.flags().is_masked()
            {
                Contents::UnsupportedEncryption
            } else {
                Contents::Directory
            };
            Ok(ZipRecord {
                path,
                member: Member {
                    is_dir,
                    is_symlink,
                    size: entry.uncompressed_size_hint(),
                    modified: zip_modified(entry.last_modified()),
                    mode: Some((mode.value() & 0o7777) as u16),
                    contents,
                },
                payload,
                span: entry.local_header_offset()..record_end,
            })
        })();
        visit(count, &shown, record)?;
    }
    if count as u64 != archive.entries_hint() {
        return Err(invalid("Недопустимая запись архива"));
    }
    Ok(())
}

fn check_zip_descriptor(
    input: &impl rawzip::ReaderAt,
    entry: &rawzip::ZipFileHeaderRecord<'_>,
    local: &rawzip::ZipLocalFileHeader<'_>,
    end: u64,
    directory: u64,
) -> io::Result<u64> {
    const SIGNATURE: [u8; 4] = 0x08074b50u32.to_le_bytes();
    let compressed = entry.compressed_size_hint();
    let uncompressed = entry.uncompressed_size_hint();
    // Seekless writers can reserve ZIP64 sizes in the local header even
    // when the final sizes fit in the central header's 32-bit fields.
    let zip64 = compressed >= u64::from(u32::MAX)
        || uncompressed >= u64::from(u32::MAX)
        || local
            .extra_fields()
            .any(|(id, _)| id == rawzip::extra_fields::ExtraFieldId::ZIP64);
    let mut expected = [0; 20];
    expected[..4].copy_from_slice(&entry.crc32().to_le_bytes());
    let width = if zip64 {
        expected[4..12].copy_from_slice(&compressed.to_le_bytes());
        expected[12..20].copy_from_slice(&uncompressed.to_le_bytes());
        20
    } else {
        expected[4..8].copy_from_slice(&(compressed as u32).to_le_bytes());
        expected[8..12].copy_from_slice(&(uncompressed as u32).to_le_bytes());
        12
    };
    let mut buffer = [0; 24];
    let length = (directory - end).min((width + SIGNATURE.len()) as u64) as usize;
    let actual = &mut buffer[..length];
    input.read_exact_at(actual, end)?;
    let body = &expected[..width];
    if actual
        .strip_prefix(&SIGNATURE)
        .is_some_and(|tail| tail.starts_with(body))
    {
        Ok(end + (width + SIGNATURE.len()) as u64)
    } else if actual.starts_with(body) {
        Ok(end + width as u64)
    } else {
        Err(invalid(
            "Размер или контрольная сумма ZIP отличаются в заголовках",
        ))
    }
}

fn zip_payload(
    entry: &rawzip::ZipFileHeaderRecord<'_>,
    index: usize,
    offset: u64,
    length: u64,
) -> Contents {
    use rawzip::CompressionMethod;
    let flags = entry.flags();
    let compression = entry.compression_method();
    let is_plain = matches!(
        compression,
        CompressionMethod::STORE | CompressionMethod::DEFLATE
    );
    if flags.has_strong_encryption() || flags.is_masked() {
        Contents::UnsupportedEncryption
    } else if flags.is_encrypted() {
        if is_plain || compression == CompressionMethod::AES {
            Contents::Encrypted(protected_zip::Member {
                index,
                header: entry.local_header_offset(),
                data: offset,
                length,
                crc: entry.crc32(),
            })
        } else {
            Contents::UnsupportedEncryption
        }
    } else if is_plain {
        Contents::Zip {
            offset,
            length,
            crc: entry.crc32(),
            deflate: compression == CompressionMethod::DEFLATE,
        }
    } else {
        Contents::UnsupportedCompression
    }
}

fn zip_name<'a>(entry: &'a rawzip::ZipFileHeaderRecord<'_>) -> io::Result<Cow<'a, str>> {
    let raw = entry.file_path().as_bytes();
    if entry.flags().is_utf8() {
        return std::str::from_utf8(raw)
            .map(Cow::Borrowed)
            .map_err(|_| invalid_path());
    }
    for (kind, data) in entry.extra_fields() {
        if kind == rawzip::extra_fields::ExtraFieldId::INFO_ZIP_UNICODE_PATH
            && let Some((1, crc)) = data.split_first()
            && let Some((checksum, utf8)) = crc.split_first_chunk::<4>()
            && u32::from_le_bytes(*checksum) == rawzip::crc32(raw)
        {
            return std::str::from_utf8(utf8)
                .map(Cow::Borrowed)
                .map_err(|_| invalid_path());
        }
    }
    if raw.is_ascii() {
        return Ok(Cow::Borrowed(std::str::from_utf8(raw).expect("ASCII path")));
    }
    // ZIP names without a Unicode flag or Unicode path field use code page 437.
    Ok(Cow::Owned(
        raw.iter()
            .map(|&byte| {
                if byte.is_ascii() {
                    char::from(byte)
                } else {
                    CP437[usize::from(byte - 128)]
                }
            })
            .collect(),
    ))
}

fn zip_modified(time: rawzip::time::ZipDateTimeKind) -> Option<SystemTime> {
    let date = chrono::NaiveDate::from_ymd_opt(
        time.year().into(),
        time.month().into(),
        time.day().into(),
    )?;
    let date = date.and_hms_nano_opt(
        time.hour().into(),
        time.minute().into(),
        time.second().into(),
        time.nanosecond(),
    )?;
    let utc = match time {
        rawzip::time::ZipDateTimeKind::Utc(_) => date.and_utc(),
        rawzip::time::ZipDateTimeKind::Local(_) => chrono::Local
            .from_local_datetime(&date)
            .earliest()?
            .to_utc(),
    };
    Some(utc.into())
}

pub(crate) fn member_path(raw: &[u8]) -> io::Result<PathBuf> {
    if raw.len() > MAX_PATH_BYTES {
        return Err(too_large());
    }
    if raw.starts_with(b"/") || raw.contains(&b'\\') || raw.iter().any(|b| b.is_ascii_control()) {
        return Err(invalid_path());
    }
    let mut path = PathBuf::new();
    for (depth, part) in raw
        .split(|&b| b == b'/')
        .filter(|part| !part.is_empty() && *part != b".")
        .enumerate()
    {
        if depth >= MAX_DEPTH {
            return Err(too_large());
        }
        if part == b".." || part.get(1) == Some(&b':') {
            return Err(invalid_path());
        }
        #[cfg(unix)]
        let name = {
            use std::os::unix::ffi::OsStrExt;
            OsStr::from_bytes(part)
        };
        #[cfg(not(unix))]
        let name = OsStr::new(std::str::from_utf8(part).map_err(|_| invalid_path())?);
        if !matches!(
            Path::new(name).components().next(),
            Some(std::path::Component::Normal(_))
        ) {
            return Err(invalid_path());
        }
        path.push(name);
    }
    Ok(path)
}

pub(crate) fn readonly() -> &'static str {
    text("Архив открыт только для просмотра")
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text(message))
}

fn invalid_path() -> io::Error {
    invalid("Недопустимый путь внутри архива")
}

fn too_large() -> io::Error {
    invalid("Слишком много данных для просмотра архива")
}

fn cancelled_read() -> io::Error {
    io::Error::other(text("Чтение архива отменено"))
}

fn open_regular(path: &Path) -> io::Result<(File, fs::Metadata)> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    if !fs::metadata(path)?.is_file() {
        return Err(invalid("Архив должен быть обычным файлом"));
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid("Архив должен быть обычным файлом"));
    }
    Ok((file, metadata))
}

// Both readers seek past ordinary payloads. Cap actual reads to bound
// central-directory work and TAR's allocations for PAX and GNU metadata.
struct MetadataReader<'a> {
    file: &'a File,
    length: u64,
    position: u64,
    remaining: usize,
    cancelled: &'a dyn Fn() -> bool,
    reading_payload: Option<&'a Cell<bool>>,
}

impl Read for MetadataReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if (self.cancelled)() {
            return Err(cancelled_read());
        }
        if self.position == self.length || buf.is_empty() {
            return Ok(0);
        }
        let charge_metadata = !self.reading_payload.is_some_and(Cell::get);
        let mut count = buf.len().min(
            (self.length - self.position)
                .try_into()
                .unwrap_or(usize::MAX),
        );
        if charge_metadata {
            count = count.min(self.remaining);
        }
        if count == 0 {
            return Err(too_large());
        }
        let read = self.file.read(&mut buf[..count])?;
        if read == 0 {
            return Err(invalid("Недопустимая запись архива"));
        }
        self.position += read as u64;
        if charge_metadata {
            self.remaining -= read;
        }
        Ok(read)
    }
}

impl Seek for MetadataReader<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        if (self.cancelled)() {
            return Err(cancelled_read());
        }
        let offset = match pos {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
            SeekFrom::End(offset) => self.length.checked_add_signed(offset),
        };
        let offset = offset
            .filter(|offset| *offset <= self.length)
            .ok_or_else(|| invalid("Недопустимая запись архива"))?;
        if offset != self.position {
            self.file.seek(SeekFrom::Start(offset))?;
            self.position = offset;
        }
        Ok(offset)
    }
}

const CP437: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ', 'Æ',
    'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú', 'ñ', 'Ñ',
    'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕',
    '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦',
    '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐',
    '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', '≡', '±',
    '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

#[cfg(test)]
mod tests {
    use super::*;
    use rawzip::path::EntryPath;
    use std::io::Write;

    fn write_zip(path: &Path, files: &[&[u8]]) {
        let mut archive = rawzip::ZipArchiveWriter::new(File::create(path).unwrap());
        archive.new_dir("empty/").create().unwrap();
        for file in files {
            let (mut entry, config) = archive
                .new_file(EntryPath::verbatim(*file))
                .start()
                .unwrap();
            let mut writer = config.wrap(&mut entry);
            writer.write_all(b"contents").unwrap();
            let (_, output) = writer.finish().unwrap();
            entry.finish(output).unwrap();
        }
        archive.finish().unwrap();
    }

    #[test]
    fn archive_zip_lists_nested_implicit_and_explicit_folders() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("example.ZIP");
        write_zip(
            &file,
            &[
                b"implicit/folder/file.txt",
                b"caf\x82.txt",
                b"legacy\\sub\\file",
            ],
        );
        let original = fs::read(&file).unwrap();
        let root = Location::Local(file.clone()).browse_archive().unwrap();
        let index = Index::load(&file, &|| false).unwrap();
        let entries = index.list(&root, &|| false).unwrap();
        let names = entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>();
        assert_eq!(names, ["café.txt", "empty", "implicit", "legacy"]);
        let folder = root.join("implicit").join("folder");
        let entries = index.list(&folder, &|| false).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, folder.join("file.txt"));
        assert_eq!(entries[0].size, 8);
        assert!(!entries[0].is_dir);
        assert!(entries[0].path.local().is_none());
        assert!(index
            .list(&root.join("empty"), &|| false)
            .unwrap()
            .is_empty());
        assert_eq!(
            index
                .list(&root.join("legacy").join("sub"), &|| false)
                .unwrap()
                .len(),
            1
        );
        assert!(index.list(&root.join("missing"), &|| false).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        assert_eq!(fs::read(&file).unwrap(), original);
    }

    #[test]
    fn archive_tar_lists_long_names_and_does_not_follow_links() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("example.tar");
        let mut archive = tar::Builder::new(File::create(&file).unwrap());
        let name = format!("folder/{}/café.txt", "a".repeat(130));
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(3);
        archive
            .append_data(&mut header, &name, &b"abc"[..])
            .unwrap();
        header.set_size(5);
        archive
            .append_data(&mut header, &name, &b"later"[..])
            .unwrap();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        archive
            .append_link(&mut header, "folder/link", "../../outside")
            .unwrap();
        let mut old = tar::Header::new_old();
        old.set_path("empty-v7/").unwrap();
        old.set_size(0);
        old.set_cksum();
        archive.append(&old, io::empty()).unwrap();
        archive.finish().unwrap();
        let root = Location::Local(file.clone()).browse_archive().unwrap();
        let index = Index::load(&file, &|| false).unwrap();
        let path = root.with_path(member_path(name.as_bytes()).unwrap());
        let entries = index.list(&path.parent().unwrap(), &|| false).unwrap();
        assert_eq!(entries[0].name, "café.txt");
        assert_eq!(entries[0].size, 5);
        let entries = index.list(&root.join("folder"), &|| false).unwrap();
        let link = entries.iter().find(|entry| entry.name == "link").unwrap();
        assert!(link.is_symlink);
        assert!(!link.is_dir);
        assert!(link.path.local().is_none());
        assert!(index
            .list(&root.join("empty-v7"), &|| false)
            .unwrap()
            .is_empty());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn archive_rejects_corrupt_metadata_and_unsafe_member_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("broken.zip");
        fs::write(&file, b"not an archive").unwrap();
        assert!(Index::load(&file, &|| false).is_err());
        write_zip(&file, &[b"valid/path"]);
        let mut data = fs::read(&file).unwrap();
        data[..4].copy_from_slice(b"NOPE");
        fs::write(&file, data).unwrap();
        assert!(Index::load(&file, &|| false).is_err());
        for name in [
            b"../outside".as_slice(),
            b"nested/../../outside",
            b"/tmp/outside",
            b"C:\\outside",
            b"\\\\server\\outside",
        ] {
            write_zip(&file, &[name]);
            assert!(Index::load(&file, &|| false).is_err());
        }
        write_zip(&file, &[b"conflict", b"conflict/child"]);
        assert!(Index::load(&file, &|| false).is_err());

        let file = dir.path().join("broken.tar");
        let mut header = tar::Header::new_gnu();
        header.set_path("entry").unwrap();
        header.set_size(2048);
        header.set_cksum();
        fs::write(&file, header.as_bytes()).unwrap();
        assert!(Index::load(&file, &|| false).is_err());

        let mut archive = tar::Builder::new(File::create(&file).unwrap());
        header.set_size(0);
        header.as_mut_bytes()[..10].copy_from_slice(b"../outside");
        header.set_cksum();
        archive.append(&header, io::empty()).unwrap();
        archive.finish().unwrap();
        assert!(Index::load(&file, &|| false).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn archive_tar_seeks_past_payload_larger_than_the_metadata_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.tar");
        let mut file = File::create(&path).unwrap();
        let size = 2 * MAX_METADATA_BYTES as u64;
        let mut header = tar::Header::new_gnu();
        header.set_path("large").unwrap();
        header.set_size(size);
        header.set_cksum();
        file.write_all(header.as_bytes()).unwrap();
        file.seek(SeekFrom::Current(size as i64)).unwrap();
        file.write_all(&[0; 1024]).unwrap();
        let root = Location::Local(path.clone()).browse_archive().unwrap();
        let index = Index::load(&path, &|| false).unwrap();
        assert_eq!(index.list(&root, &|| false).unwrap()[0].size, size);
        assert!(Index::load(&path, &|| true).is_err());
    }

    #[test]
    fn archive_address_round_trip_and_vfs_readonly_guard() {
        let dir = tempfile::tempdir().unwrap();
        let local = Location::Local(dir.path().to_owned());
        let archive = local.join("actual ! name%.ZIP").browse_archive().unwrap();
        let member = archive.join("folder !%").join("example.txt");
        assert_eq!(
            crate::vfs::resolve(&member.display(), &local).unwrap(),
            member
        );
        assert_eq!(member.parent(), Some(archive.join("folder !%")));
        assert_eq!(archive.parent(), Some(local.clone()));
        assert!(crate::vfs::resolve(&format!("{}../outside", archive.display()), &local).is_err());
        let source = local.join("source");
        fs::write(source.local().unwrap(), "example").unwrap();
        assert!(crate::vfs::trash_entries(std::slice::from_ref(&member)).is_err());
        assert!(crate::vfs::transfer(
            std::slice::from_ref(&member),
            &local,
            crate::fs_ops::TransferKind::Copy
        )
        .is_err());
        assert!(crate::vfs::transfer(
            std::slice::from_ref(&source),
            &archive,
            crate::fs_ops::TransferKind::Move
        )
        .is_err());
        assert!(crate::vfs::create_directory(&archive, "new").is_err());
        assert!(crate::vfs::rename_entry(&member, "renamed").is_err());
        assert_eq!(fs::read(source.local().unwrap()).unwrap(), b"example");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
