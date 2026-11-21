use super::{
    extraction::Extraction, integrity::check_tar_trailer, invalid, invalid_path, member_path,
    protected_zip::zip_error, scan_zip, too_large, ArchiveWriter, Contents, Format, Index, Member,
    MetadataReader, MAX_ENTRIES, MAX_INDEX_BYTES, MAX_METADATA_BYTES,
};
use crate::operations::{Progress, TransferError};
use std::{
    cell::Cell,
    collections::BTreeSet,
    io::{self, Seek, SeekFrom, Write},
    ops::Range,
    path::{Path, PathBuf},
};

pub(crate) enum MemberEdit<'a> {
    Add(&'a Path),
    Remove(BTreeSet<&'a Path>),
    Rename { from: &'a Path, to: &'a Path },
}

impl MemberEdit<'_> {
    fn removes(&self, path: &Path) -> bool {
        matches!(self, Self::Remove(roots) if path.ancestors().any(|ancestor| roots.contains(ancestor)))
    }

    fn renamed(&self, path: &Path) -> Option<PathBuf> {
        let Self::Rename { from, to } = self else {
            return None;
        };
        let rest = path.strip_prefix(from).ok()?;
        if rest.as_os_str().is_empty() {
            Some(to.to_path_buf())
        } else {
            Some(to.join(rest))
        }
    }

    fn validate(&self, index: &Index) -> io::Result<()> {
        let validate_path = |path: &Path| {
            if member_path(path.as_os_str().as_encoded_bytes())? != path {
                return Err(invalid_path());
            }
            Ok(())
        };
        let require_entry = |path: &Path| {
            validate_path(path)?;
            if !member_exists(index, path) {
                return Err(missing_entry(path));
            }
            Ok(())
        };
        match self {
            Self::Add(folder) => {
                validate_path(folder)?;
                if !index.folders.contains_key(*folder) {
                    return Err(missing_entry(folder));
                }
            }
            Self::Remove(roots) => {
                if roots.is_empty() {
                    return Err(invalid_path());
                }
                for root in roots {
                    require_entry(root)?;
                }
            }
            Self::Rename { from, to } => {
                require_entry(from)?;
                validate_path(to)?;
                if to.parent().is_none() || from.parent() != to.parent() {
                    return Err(invalid_path());
                }
                if member_exists(index, to) {
                    return Err(occupied(to));
                }
            }
        }
        Ok(())
    }
}

pub(crate) struct EditableArchive {
    source: Extraction<'static>,
    format: Format,
    records: Vec<Record>,
    zip_entries: usize,
    protected: bool,
}

struct Record {
    path: Option<PathBuf>,
    renamed: Option<PathBuf>,
    directory: bool,
    kind: RecordKind,
}

enum RecordKind {
    Zip {
        index: usize,
        offset: u64,
        size: u64,
        payload: Contents,
    },
    Tar {
        original: Range<u64>,
        rewrite: Option<Box<TarRewrite>>,
    },
}

struct TarRewrite {
    header: tar::Header,
    data: Range<u64>,
    pax: Vec<(String, Vec<u8>)>,
}

impl EditableArchive {
    pub(crate) fn open(
        path: &Path,
        format: Format,
        edit: &MemberEdit<'_>,
        progress: &mut Progress,
    ) -> Result<Self, TransferError> {
        let cancel = progress.cancellation();
        cancel.check()?;
        let source = Extraction::open(path, None)?;
        let (file, length) = source.input();
        let cancelled = || cancel.is_requested();
        let input = MetadataReader {
            file,
            length,
            position: 0,
            remaining: MAX_METADATA_BYTES,
            cancelled: &cancelled,
            reading_payload: None,
        };
        let (mut records, protected) = match format {
            Format::Zip => read_zip(input)?,
            Format::Tar => (read_tar(input, path, edit, progress)?, false),
        };
        source.verify_source()?;
        edit.validate(&layout(
            records
                .iter()
                .filter_map(|r| Some((r.path.as_deref()?, r.directory))),
        )?)?;
        let zip_entries = records.len();
        records.retain_mut(|record| {
            if let Some(path) = &record.path {
                if edit.removes(path) {
                    return false;
                }
                record.renamed = edit.renamed(path);
            }
            true
        });
        Ok(Self {
            source,
            format,
            records,
            zip_entries,
            protected,
        })
    }

    pub(crate) fn validate_additions<'a>(
        &'a self,
        additions: impl Iterator<Item = (&'a Path, bool)> + Clone,
    ) -> io::Result<()> {
        let retained = layout(self.names())?;
        for (path, _) in additions.clone() {
            if member_exists(&retained, path) {
                return Err(occupied(path));
            }
        }
        layout(self.names().chain(additions)).map(|_| ())
    }

    pub(crate) fn validate_output<'a>(
        &'a self,
        path: &Path,
        additions: impl Iterator<Item = (&'a Path, bool)>,
        progress: &mut Progress,
    ) -> Result<(), TransferError> {
        let output = Self::open(path, self.format, &MemberEdit::Add(Path::new("")), progress)?;
        let mut actual = output.names();
        let matches = self
            .names()
            .chain(additions)
            .all(|(expected_path, expected_dir)| {
                actual
                    .next()
                    .is_some_and(|(path, dir)| path == expected_path && dir == expected_dir)
            });
        if !matches || actual.next().is_some() {
            return Err(invalid(crate::i18n::key!(
                "Список записей пересобранного архива не совпадает"
            ))
            .into());
        }
        Ok(())
    }

    fn names(&self) -> impl Iterator<Item = (&Path, bool)> {
        self.records.iter().filter_map(|record| {
            Some((
                record.renamed.as_deref().or(record.path.as_deref())?,
                record.directory,
            ))
        })
    }

    pub(crate) fn has_protected_entries(&self) -> bool {
        self.protected
    }

    pub(crate) fn verify_source(&self) -> Result<(), TransferError> {
        self.source.verify_source()
    }

    pub(crate) fn write_existing<W: Write>(
        &self,
        writer: &mut ArchiveWriter<'_, W>,
        progress: &mut Progress,
    ) -> Result<(), TransferError> {
        match (self.format, writer) {
            (Format::Zip, ArchiveWriter::StreamZip { writer, .. }) => {
                self.write_zip(writer, progress)?
            }
            (Format::Tar, ArchiveWriter::Tar(writer)) => self.write_tar(writer, progress)?,
            _ => {
                return Err(invalid(crate::i18n::key!("Неподдерживаемый формат пересборки")).into())
            }
        }
        Ok(())
    }

    fn write_zip<W: Write>(
        &self,
        writer: &mut zip::ZipWriter<zip::write::StreamWriter<W>>,
        progress: &mut Progress,
    ) -> io::Result<()> {
        // Read and check the retained plain entries before copying their encoded bytes.
        for record in &self.records {
            progress.check_cancelled()?;
            let RecordKind::Zip { size, payload, .. } = record.kind else {
                unreachable!()
            };
            match payload {
                Contents::Zip { .. } => {
                    self.source.check_payload(
                        payload,
                        size,
                        record.path.as_deref().unwrap(),
                        progress,
                    )?;
                }
                Contents::Encrypted(_) => {}
                _ => {
                    return Err(invalid(crate::i18n::key!(
                        "Редактирование этого метода ZIP не поддерживается"
                    )))
                }
            }
        }
        let cancel = progress.cancellation();
        let (file, length) = self.source.input();
        let cancelled = || cancel.is_requested();
        let copying = Cell::new(false);
        let mut zip = zip::ZipArchive::new(MetadataReader {
            file,
            length,
            position: 0,
            remaining: MAX_METADATA_BYTES,
            cancelled: &cancelled,
            reading_payload: Some(&copying),
        })
        .map_err(zip_error)?;
        // The second reader keys its records by name. Comparing its count and
        // the exact local offsets prevents silently losing or copying aliases.
        if zip.len() != self.zip_entries || zip.offset() != 0 {
            return Err(invalid(crate::i18n::key!(
                "Структура ZIP не поддерживает безопасную пересборку"
            )));
        }
        writer
            .set_raw_comment(zip.comment().into())
            .map_err(zip_error)?;
        for record in &self.records {
            cancel.check()?;
            let RecordKind::Zip {
                index,
                offset,
                size,
                payload,
            } = record.kind
            else {
                unreachable!()
            };
            progress.item(record.path.as_deref().unwrap());
            let entry = zip.by_index_raw(index).map_err(zip_error)?;
            let (length, crc, encrypted) = match payload {
                Contents::Zip { length, crc, .. } => (length, crc, false),
                Contents::Encrypted(member) => (member.length, member.crc, true),
                _ => unreachable!(),
            };
            if entry.header_start() != offset
                || entry.size() != size
                || entry.compressed_size() != length
                || entry.crc32() != crc
                || entry.encrypted() != encrypted
            {
                return Err(invalid(crate::i18n::key!(
                    "Структура ZIP не поддерживает безопасную пересборку"
                )));
            }
            copying.set(true);
            if let Some(name) = &record.renamed {
                let name = name.to_str().ok_or_else(invalid_path)?;
                writer.raw_copy_file_rename(
                    entry,
                    format!("{name}{}", if record.directory { "/" } else { "" }),
                )
            } else {
                writer.raw_copy_file(entry)
            }
            .map_err(zip_error)?;
            copying.set(false);
        }
        Ok(())
    }

    fn write_tar<W: Write>(
        &self,
        writer: &mut tar::Builder<W>,
        progress: &mut Progress,
    ) -> io::Result<()> {
        let (file, _) = self.source.input();
        for record in &self.records {
            let RecordKind::Tar { original, rewrite } = &record.kind else {
                unreachable!()
            };
            let shown = record.path.as_deref().unwrap_or_else(|| Path::new("."));
            let range = rewrite.as_ref().map_or(original, |rewrite| &rewrite.data);
            let mut input = file;
            input.seek(SeekFrom::Start(range.start))?;
            let expected = range.end - range.start;
            let actual = progress.read_stream(shown, expected, input, |reader| {
                if let Some(rewrite) = rewrite {
                    let mut header = rewrite.header.clone();
                    writer.append_pax_extensions(
                        rewrite
                            .pax
                            .iter()
                            .map(|(key, value)| (key.as_str(), value.as_slice())),
                    )?;
                    writer.append_data(
                        &mut header,
                        record.renamed.as_deref().unwrap_or(shown),
                        reader,
                    )
                } else {
                    io::copy(reader, writer.get_mut()).map(|_| ())
                }
            })?;
            if actual != expected {
                return Err(invalid(crate::i18n::key!(
                    "Размер данных записи архива не совпадает"
                )));
            }
        }
        Ok(())
    }
}

fn layout<'a>(members: impl Iterator<Item = (&'a Path, bool)>) -> io::Result<Index> {
    let mut index = Index::default();
    index.folders.insert(PathBuf::new(), Default::default());
    let mut names = BTreeSet::new();
    for (path, directory) in members {
        if !names.insert(path) {
            return Err(occupied(path));
        }
        let raw = path.as_os_str().as_encoded_bytes();
        if member_path(raw)? != path || (path.as_os_str().is_empty() && !directory) {
            return Err(invalid_path());
        }
        index.add(
            path,
            Member {
                is_dir: directory,
                ..Member::default()
            },
        )?;
    }
    Ok(index)
}

fn member_exists(index: &Index, path: &Path) -> bool {
    path.parent()
        .and_then(|parent| index.folders.get(parent))
        .zip(path.file_name())
        .is_some_and(|(children, name)| children.contains_key(name))
}

fn occupied(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        crate::tr_format!(
            "Путь уже используется в архиве: {}",
            "Path is already used in the archive: {}",
            path.display()
        ),
    )
}

fn read_zip(input: MetadataReader<'_>) -> io::Result<(Vec<Record>, bool)> {
    let mut records = Vec::new();
    let mut ranges = Vec::new();
    let mut protected = false;
    let mut path_bytes = 0;
    scan_zip(input, true, |index, _, record| {
        let record = record?;
        path_bytes += record.path.as_os_str().len();
        if path_bytes > MAX_INDEX_BYTES {
            return Err(too_large());
        }
        protected |= matches!(
            record.payload,
            Contents::Encrypted(_) | Contents::UnsupportedEncryption
        );
        records.push(Record {
            path: Some(record.path),
            renamed: None,
            directory: record.member.is_dir,
            kind: RecordKind::Zip {
                index: index - 1,
                offset: record.span.start,
                size: record.member.size,
                payload: record.payload,
            },
        });
        ranges.push(record.span);
        Ok(())
    })?;
    ranges.sort_unstable_by_key(|range| range.start);
    if ranges.first().is_some_and(|range| range.start != 0)
        || ranges
            .windows(2)
            .any(|ranges| ranges[0].end > ranges[1].start)
    {
        return Err(invalid(crate::i18n::key!(
            "Структура ZIP не поддерживает безопасную пересборку"
        )));
    }
    Ok((records, protected))
}

fn read_tar(
    input: MetadataReader<'_>,
    path: &Path,
    edit: &MemberEdit<'_>,
    progress: &mut Progress,
) -> io::Result<Vec<Record>> {
    const BLOCK: u64 = 512;
    let mut archive = tar::Archive::new(input);
    let mut records = Vec::new();
    let mut previous_end = 0;
    let mut path_bytes = 0;
    for entry in archive.entries_with_seek()? {
        progress.check_cancelled()?;
        if records.len() >= MAX_ENTRIES {
            return Err(too_large());
        }
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_gnu_sparse() {
            return Err(invalid(crate::i18n::key!(
                "Пересборка разреженного TAR не поддерживается"
            )));
        }
        let data_start = entry.raw_file_position();
        let size = entry.size();
        let data_end = data_start.checked_add(size).ok_or_else(too_large)?;
        let end = data_end
            .checked_next_multiple_of(BLOCK)
            .ok_or_else(too_large)?;
        let original = previous_end..end;
        previous_end = end;
        let raw_name = entry.path_bytes();
        let directory = kind.is_dir()
            || (kind.is_file() && entry.header().as_ustar().is_none() && raw_name.ends_with(b"/"));
        let member = (!kind.is_pax_global_extensions())
            .then(|| member_path(&raw_name))
            .transpose()?;
        path_bytes += member.as_ref().map_or(0, |path| path.as_os_str().len());
        if path_bytes > MAX_INDEX_BYTES {
            return Err(too_large());
        }
        let renamed = member.as_deref().and_then(|path| edit.renamed(path));
        let link = entry.link_name_bytes().map(|link| link.into_owned());
        let mut new_link = None;
        if kind.is_hard_link() && !member.as_deref().is_some_and(|path| edit.removes(path)) {
            let link_path = member_path(link.as_deref().ok_or_else(invalid_path)?)?;
            if edit.removes(&link_path) {
                return Err(invalid(crate::i18n::key!(
                    "В TAR есть жёсткая ссылка на удаляемую запись"
                )));
            }
            new_link = edit.renamed(&link_path);
        }
        let mut header = entry.header().clone();
        let rewriting = renamed.is_some() || new_link.is_some();
        let mut pax = Vec::new();
        if let Some(extensions) = entry.pax_extensions()? {
            for field in extensions {
                let field = field.map_err(|_| {
                    invalid(crate::i18n::key!("Повреждён расширенный заголовок TAR"))
                })?;
                let key = field.key().map_err(|_| {
                    invalid(crate::i18n::key!("Повреждён расширенный заголовок TAR"))
                })?;
                if key.starts_with("GNU.sparse.") {
                    return Err(invalid(crate::i18n::key!(
                        "Пересборка разреженного TAR не поддерживается"
                    )));
                }
                if kind.is_pax_global_extensions() && matches!(key, "path" | "linkpath" | "size") {
                    return Err(invalid(crate::i18n::key!(
                        "Глобальные пути или размеры TAR нельзя безопасно пересобрать"
                    )));
                }
                if rewriting && key != "path" && key != "linkpath" {
                    pax.push((key.to_owned(), field.value_bytes().to_owned()));
                }
            }
        }
        let rewrite = rewriting.then(|| {
            header.set_size(size);
            if directory {
                header.set_entry_type(tar::EntryType::Directory);
            }
            // A GNU long-link or a PAX link may not be in the ordinary header.
            // Preserve its effective bytes, updating internal hard links as needed.
            if let Some(link) = new_link
                .as_ref()
                .map(|path| path.as_os_str().as_encoded_bytes())
                .or(link.as_deref())
                && (new_link.is_some() || header.link_name_bytes().as_deref() != Some(link))
            {
                pax.push(("linkpath".into(), link.to_owned()));
            }
            Box::new(TarRewrite {
                header,
                data: data_start..data_end,
                pax,
            })
        });
        records.push(Record {
            path: member,
            renamed,
            directory,
            kind: RecordKind::Tar { original, rewrite },
        });
    }
    check_tar_trailer(archive.into_inner(), path, progress)?;
    Ok(records)
}

fn missing_entry(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        crate::tr_format!(
            "В архиве больше нет записи {}",
            "Archive entry no longer exists: {}",
            path.display()
        ),
    )
}
