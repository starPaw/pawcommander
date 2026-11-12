//! Read-only archive directory indexes. Member names never enter host I/O.
use crate::{
    fs_ops::ListingMetadata,
    i18n::text,
    vfs::{FileEntry, Location},
};
use chrono::TimeZone;
use std::{
    borrow::Cow,
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_ENTRIES: usize = 50_000;
const MAX_INDEX_BYTES: usize = 12 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 32 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_DEPTH: usize = 64;

#[derive(Clone, Copy)]
pub(crate) enum Format {
    Zip,
    Tar,
}

impl Format {
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
        let mut index = Self::default();
        index.folders.insert(PathBuf::new(), BTreeMap::new());
        if cancelled() {
            return Err(cancelled_read().to_string());
        }
        let result = open_regular(file).and_then(|(file, length)| {
            let input = MetadataReader {
                file,
                length,
                position: 0,
                remaining: MAX_METADATA_BYTES,
                cancelled,
            };
            match format {
                Format::Zip => index.read_zip(input),
                Format::Tar => index.read_tar(input),
            }
        });
        result.map_err(|error| {
            crate::tr_format!(
                "Не удалось прочитать архив «{}»: {error}",
                "Could not read archive “{}”: {error}",
                file.display()
            )
        })?;
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
        let cancelled = input.cancelled;
        // The three variable-length fields in a central-directory record
        // can each be up to 65535 bytes.
        let mut buffer = vec![0; 3 * usize::from(u16::MAX)];
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
            let name = zip_name(&entry)?;
            let name = if name.contains('\\') {
                Cow::Owned(name.replace('\\', "/"))
            } else {
                name
            };
            let mode = entry.mode();
            let is_symlink = mode.is_symlink();
            let is_dir =
                !is_symlink && (mode.value() & 0o170000 == 0o040000 || name.ends_with('/'));
            let directory_start = archive.directory_offset();
            if entry.local_header_offset() >= directory_start {
                return Err(invalid("Недопустимая запись архива"));
            }
            let local = archive
                .get_entry(entry.wayfinder())
                .map_err(io::Error::other)?;
            if local.compressed_data_range().1 > directory_start {
                return Err(invalid("Недопустимая запись архива"));
            }
            self.add(
                &member_path(name.as_bytes())?,
                Member {
                    is_dir,
                    is_symlink,
                    size: entry.uncompressed_size_hint(),
                    modified: zip_modified(entry.last_modified()),
                    mode: Some((mode.value() & 0o7777) as u16),
                },
            )?;
        }
        // Our entry limit is below the count at which legacy ZIP counts wrap.
        if count as u64 != archive.entries_hint() {
            return Err(invalid("Недопустимая запись архива"));
        }
        Ok(())
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
            let entry = entry?;
            let header = entry.header();
            let kind = header.entry_type();
            if kind.is_pax_global_extensions() {
                continue;
            }
            let name = entry.path_bytes();
            let path = member_path(&name)?;
            self.add(
                &path,
                Member {
                    is_dir: kind.is_dir()
                        || (kind.is_file() && header.as_ustar().is_none() && name.ends_with(b"/")),
                    is_symlink: kind.is_symlink(),
                    size: entry.size(),
                    modified: header
                        .mtime()
                        .ok()
                        .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs))),
                    mode: header.mode().ok().map(|mode| (mode & 0o7777) as u16),
                },
            )?;
        }
        Ok(())
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
        if kind == rawzip::extra_fields::ExtraFieldId::INFO_ZIP_UNICODE_PATH {
            if let Some((1, crc)) = data.split_first() {
                if let Some((checksum, utf8)) = crc.split_first_chunk::<4>() {
                    if u32::from_le_bytes(*checksum) == rawzip::crc32(raw) {
                        return std::str::from_utf8(utf8)
                            .map(Cow::Borrowed)
                            .map_err(|_| invalid_path());
                    }
                }
            }
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

fn open_regular(path: &Path) -> io::Result<(File, u64)> {
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
    Ok((file, metadata.len()))
}

// Both readers seek past ordinary payloads. Cap actual reads to bound
// central-directory work and TAR's allocations for PAX and GNU metadata.
struct MetadataReader<'a> {
    file: File,
    length: u64,
    position: u64,
    remaining: usize,
    cancelled: &'a dyn Fn() -> bool,
}

impl Read for MetadataReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if (self.cancelled)() {
            return Err(cancelled_read());
        }
        if self.position == self.length || buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(too_large());
        }
        let count = buf.len().min(self.remaining).min(
            (self.length - self.position)
                .try_into()
                .unwrap_or(usize::MAX),
        );
        let read = self.file.read(&mut buf[..count])?;
        if read == 0 {
            return Err(invalid("Недопустимая запись архива"));
        }
        self.position += read as u64;
        self.remaining -= read;
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
