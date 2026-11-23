use super::{
    extraction::{archive_read_error, Extraction},
    integrity::check_tar_trailer,
    invalid_path, member_path, parse_member_path,
    protected_zip::zip_error,
    scan_zip, too_large, ArchiveIssue, ArchiveWriter, Contents, Format, Index, Member,
    MetadataReader, MAX_ENTRIES, MAX_INDEX_BYTES, MAX_METADATA_BYTES,
};
use crate::{
    file_error::{AccessError, ArchiveError, ArchiveMemberError},
    operations::{Progress, TransferError},
    vfs::Location,
};
use std::{
    cell::Cell,
    collections::BTreeSet,
    io::{self, Seek, SeekFrom, Write},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
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

    fn validate(&self, root: &Location, index: &Index) -> Result<(), TransferError> {
        let require_entry = |path: &Path| {
            validate_path(root, path)?;
            if !member_exists(index, path) {
                return Err(TransferError::from(ArchiveError::MemberMissing(
                    root.join(path),
                )));
            }
            Ok(())
        };
        match self {
            Self::Add(folder) => {
                validate_path(root, folder)?;
                if !index.folders.contains_key(*folder) {
                    return Err(ArchiveError::MemberMissing(root.join(folder)).into());
                }
            }
            Self::Remove(roots) => {
                if roots.is_empty() {
                    return Err(ArchiveError::SelectArchiveEntries.into());
                }
                for root in roots {
                    require_entry(root)?;
                }
            }
            Self::Rename { from, to } => {
                require_entry(from)?;
                validate_path(root, to)?;
                if to.parent().is_none() || from.parent() != to.parent() {
                    return Err(path_error(root, to, ArchiveMemberError::InvalidPath));
                }
                if member_exists(index, to) {
                    return Err(ArchiveError::MemberOccupied(root.join(to)).into());
                }
            }
        }
        Ok(())
    }
}

pub(crate) struct EditableArchive {
    source: Extraction<'static>,
    root: Location,
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
        let root = Location::Archive {
            archive: Arc::new(path.to_owned()),
            path: PathBuf::new(),
        };
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
            Format::Zip => read_zip(input),
            Format::Tar => read_tar(input, path, edit, progress).map(|records| (records, false)),
        }
        .map_err(|error| archive_read_error(path, error))?;
        source.verify_source()?;
        edit.validate(
            &root,
            &layout(
                &root,
                records
                    .iter()
                    .filter_map(|r| Some((r.path.as_deref()?, r.directory))),
            )?,
        )?;
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
            root,
            format,
            records,
            zip_entries,
            protected,
        })
    }

    pub(crate) fn validate_additions<'a>(
        &'a self,
        additions: impl Iterator<Item = (&'a Path, bool)> + Clone,
    ) -> Result<(), TransferError> {
        let retained = layout(&self.root, self.names())?;
        for (path, _) in additions.clone() {
            if member_exists(&retained, path) {
                return Err(ArchiveError::MemberOccupied(self.root.join(path)).into());
            }
        }
        layout(&self.root, self.names().chain(additions)).map(|_| ())
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
            return Err(ArchiveError::Rebuild {
                location: self.source.location(),
                source: ArchiveIssue::RebuiltEntryListMismatch.io_error(),
            }
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
                self.write_zip(writer, progress)
            }
            (Format::Tar, ArchiveWriter::Tar(writer)) => self.write_tar(writer, progress),
            _ => Err(self.rebuild_error(None, ArchiveIssue::UnsupportedRebuildFormat.io_error())),
        }
    }

    fn rebuild_error(&self, member: Option<&Path>, source: io::Error) -> TransferError {
        ArchiveError::Rebuild {
            location: member.map_or_else(|| self.source.location(), |path| self.root.join(path)),
            source,
        }
        .into()
    }

    fn write_zip<W: Write>(
        &self,
        writer: &mut zip::ZipWriter<zip::write::StreamWriter<W>>,
        progress: &mut Progress,
    ) -> Result<(), TransferError> {
        // Read and check the retained plain entries before copying their encoded bytes.
        for record in &self.records {
            progress.check_cancelled()?;
            let RecordKind::Zip { size, payload, .. } = record.kind else {
                unreachable!()
            };
            let path = record.path.as_deref().unwrap();
            match payload {
                Contents::Zip { .. } => {
                    self.source
                        .check_payload(payload, size, path, progress)
                        .map_err(|source| AccessError::read(&self.root.join(path), source))?;
                }
                Contents::Encrypted(_) => {}
                _ => {
                    return Err(self.rebuild_error(
                        Some(path),
                        ArchiveIssue::UnsupportedZipEditMethod.io_error(),
                    ))
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
        .map_err(|error| self.rebuild_error(None, zip_error(error)))?;
        // The second reader keys its records by name. Comparing its count and
        // the exact local offsets prevents silently losing or copying aliases.
        if zip.len() != self.zip_entries || zip.offset() != 0 {
            return Err(self.rebuild_error(None, ArchiveIssue::UnsafeZipRebuild.io_error()));
        }
        writer
            .set_raw_comment(zip.comment().into())
            .map_err(|error| self.rebuild_error(None, zip_error(error)))?;
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
            let path = record.path.as_deref().unwrap();
            let failure = |source| self.rebuild_error(Some(path), source);
            progress.item(path);
            let entry = zip
                .by_index_raw(index)
                .map_err(|error| failure(zip_error(error)))?;
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
                return Err(failure(ArchiveIssue::UnsafeZipRebuild.io_error()));
            }
            copying.set(true);
            if let Some(name) = &record.renamed {
                let name = name
                    .to_str()
                    .ok_or_else(|| path_error(&self.root, name, ArchiveMemberError::InvalidPath))?;
                writer.raw_copy_file_rename(
                    entry,
                    format!("{name}{}", if record.directory { "/" } else { "" }),
                )
            } else {
                writer.raw_copy_file(entry)
            }
            .map_err(|error| failure(zip_error(error)))?;
            copying.set(false);
        }
        Ok(())
    }

    fn write_tar<W: Write>(
        &self,
        writer: &mut tar::Builder<W>,
        progress: &mut Progress,
    ) -> Result<(), TransferError> {
        let (file, _) = self.source.input();
        for record in &self.records {
            let RecordKind::Tar { original, rewrite } = &record.kind else {
                unreachable!()
            };
            let shown = record.path.as_deref().unwrap_or_else(|| Path::new("."));
            let failure = |source| self.rebuild_error(record.path.as_deref(), source);
            let range = rewrite.as_ref().map_or(original, |rewrite| &rewrite.data);
            let mut input = file;
            input.seek(SeekFrom::Start(range.start)).map_err(failure)?;
            let expected = range.end - range.start;
            let actual = progress
                .read_stream(shown, expected, input, |reader| {
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
                })
                .map_err(failure)?;
            if actual != expected {
                return Err(failure(ArchiveIssue::EntrySizeMismatch.io_error()));
            }
        }
        Ok(())
    }
}

fn validate_path(root: &Location, path: &Path) -> Result<(), TransferError> {
    let normalized = parse_member_path(path.as_os_str().as_encoded_bytes())
        .map_err(|source| path_error(root, path, source))?;
    if normalized != path {
        return Err(path_error(root, path, ArchiveMemberError::InvalidPath));
    }
    Ok(())
}

fn path_error(root: &Location, path: &Path, source: ArchiveMemberError) -> TransferError {
    ArchiveError::EntryPath {
        entry: root.join(path),
        source,
    }
    .into()
}

fn layout<'a>(
    root: &Location,
    members: impl Iterator<Item = (&'a Path, bool)>,
) -> Result<Index, TransferError> {
    let mut index = Index::default();
    index.folders.insert(PathBuf::new(), Default::default());
    let mut names = BTreeSet::new();
    for (path, directory) in members {
        if !names.insert(path) {
            return Err(ArchiveError::MemberOccupied(root.join(path)).into());
        }
        validate_path(root, path)?;
        if path.as_os_str().is_empty() && !directory {
            return Err(path_error(root, path, ArchiveMemberError::InvalidPath));
        }
        index
            .add(
                path,
                Member {
                    is_dir: directory,
                    ..Member::default()
                },
            )
            .map_err(|source| AccessError::read(&root.join(path), source))?;
    }
    Ok(index)
}

fn member_exists(index: &Index, path: &Path) -> bool {
    path.parent()
        .and_then(|parent| index.folders.get(parent))
        .zip(path.file_name())
        .is_some_and(|(children, name)| children.contains_key(name))
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
        return Err(ArchiveIssue::UnsafeZipRebuild.io_error());
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
            return Err(ArchiveIssue::UnsupportedSparseTarRebuild.io_error());
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
                return Err(ArchiveIssue::TarHardLinkToRemovedEntry.io_error());
            }
            new_link = edit.renamed(&link_path);
        }
        let mut header = entry.header().clone();
        let rewriting = renamed.is_some() || new_link.is_some();
        let mut pax = Vec::new();
        if let Some(extensions) = entry.pax_extensions()? {
            for field in extensions {
                let field = field
                    .map_err(|source| ArchiveIssue::InvalidTarExtendedHeader.with_source(source))?;
                let key = field
                    .key()
                    .map_err(|source| ArchiveIssue::InvalidTarExtendedHeader.with_source(source))?;
                if key.starts_with("GNU.sparse.") {
                    return Err(ArchiveIssue::UnsupportedSparseTarRebuild.io_error());
                }
                if kind.is_pax_global_extensions() && matches!(key, "path" | "linkpath" | "size") {
                    return Err(ArchiveIssue::UnsafeTarGlobalMetadata.io_error());
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
