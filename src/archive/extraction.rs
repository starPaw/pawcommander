use super::protected_zip::{password_required, EntryReader, ProtectedZip};
use super::{open_regular, ArchiveIssue, Contents, Format, Index, Member};
use crate::{
    file_error::{AccessAction, AccessError, ArchiveError, ArchiveMemberError},
    fs_ops,
    operations::{Cancellation, Progress, TransferError},
    vfs::Location,
};
use flate2::{CrcReader, Decompress, FlushDecompress, Status};
use std::{
    collections::BTreeSet,
    fs::{self, File, Metadata},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

const MAX_OUTPUT_BYTES: u64 = 32 * 1024 * 1024 * 1024;

pub(crate) struct Extraction<'a> {
    path: PathBuf,
    file: File,
    stamp: Metadata,
    protected: ProtectedZip,
    password: Option<&'a str>,
    #[cfg(target_os = "macos")]
    mount: crate::smb::MutationMount,
}

pub(crate) struct SelectedTree {
    pub source: Location,
    pub entries: Vec<SelectedEntry>,
    pub bytes: u64,
}

pub(crate) struct SelectedEntry {
    pub relative: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
    contents: Contents,
}

impl SelectedEntry {
    pub fn is_dir(&self) -> bool {
        matches!(self.contents, Contents::Directory)
    }
}

impl<'a> Extraction<'a> {
    pub(super) fn open(path: &Path, password: Option<&'a str>) -> Result<Self, TransferError> {
        #[cfg(target_os = "macos")]
        let mount = {
            use crate::{file_error::SmbMutationAction, smb::MutationMount};

            let mut mount = MutationMount::new(SmbMutationAction::ReadArchive);
            mount.check(path)?;
            let resolved = path.canonicalize().ok().or_else(|| {
                path.parent()
                    .zip(path.file_name())
                    .and_then(|(parent, name)| {
                        parent.canonicalize().ok().map(|parent| parent.join(name))
                    })
            });
            if let Some(resolved) = resolved.filter(|resolved| resolved != path) {
                mount.check(&resolved)?;
            }
            mount
        };
        let (file, stamp) = open_regular(path).map_err(|source| {
            AccessError::new(
                &Location::Local(path.to_owned()),
                AccessAction::OpenFile,
                source,
            )
        })?;
        #[cfg(target_os = "macos")]
        mount.verify_file(&file, path)?;
        Ok(Self {
            path: path.into(),
            file,
            stamp,
            protected: ProtectedZip::default(),
            password: password.filter(|password| !password.is_empty()),
            #[cfg(target_os = "macos")]
            mount,
        })
    }

    pub(super) fn input(&self) -> (&File, u64) {
        (&self.file, self.stamp.len())
    }

    pub(super) fn location(&self) -> Location {
        Location::Local(self.path.clone())
    }

    pub(super) fn prepare_protected(
        &mut self,
        entries: impl Iterator<Item = (super::protected_zip::Member, u64)>,
        cancel: &Cancellation,
    ) -> io::Result<()> {
        let prepared = ProtectedZip::read(&self.file, self.stamp.len(), entries, cancel);
        cancel.check()?;
        self.protected = prepared?;
        Ok(())
    }

    pub(crate) fn prepare(
        sources: &[Location],
        password: Option<&'a str>,
        cancel: &Cancellation,
    ) -> Result<(Self, Vec<SelectedTree>), TransferError> {
        let first = sources.first().ok_or(ArchiveError::SelectArchiveEntries)?;
        let path = first
            .archive_source()
            .ok_or(ArchiveError::SelectArchiveEntries)?;
        let parent = first
            .path()
            .parent()
            .ok_or(ArchiveError::SelectArchiveEntries)?;
        let format = Format::from_path(path)
            .ok_or_else(|| ArchiveError::UnsupportedFormat(Location::Local(path.to_owned())))?;
        if sources.iter().any(|source| {
            source.archive_source() != Some(path) || source.path().parent() != Some(parent)
        }) {
            return Err(ArchiveError::SelectArchiveEntries.into());
        }
        cancel.check()?;
        let mut archive = Self::open(path, password)?;
        let (file, length) = archive.input();
        let parsed = Index::read(file, length, format, &|| cancel.is_requested());
        cancel.check()?;
        let index = parsed.map_err(|error| archive_read_error(path, error))?;
        archive.verify_source()?;

        let mut budget = MAX_OUTPUT_BYTES;
        let mut seen = BTreeSet::new();
        let mut trees = Vec::new();
        let folder = index
            .folders
            .get(parent)
            .ok_or_else(|| ArchiveError::MemberMissing(first.with_path(parent.to_owned())))?;
        for source in sources {
            cancel.check()?;
            let name = source
                .path()
                .file_name()
                .ok_or(ArchiveError::SelectArchiveEntries)?;
            if !seen.insert(name) {
                continue;
            }
            let member = folder
                .get(name)
                .ok_or_else(|| ArchiveError::MemberMissing(source.clone()))?;
            let mut plan = Selection {
                index: &index,
                source,
                cancel,
                budget: &mut budget,
                entries: Vec::new(),
            };
            let start = *plan.budget;
            plan.visit(PathBuf::new(), *member)?;
            trees.push(SelectedTree {
                source: source.clone(),
                entries: plan.entries,
                bytes: start - budget,
            });
        }
        let mut encrypted = trees
            .iter()
            .flat_map(|tree| &tree.entries)
            .filter_map(|entry| match entry.contents {
                Contents::Encrypted(member) => Some((member, entry.size)),
                _ => None,
            })
            .peekable();
        if let Some(&(first, _)) = encrypted.peek() {
            let password = archive
                .password
                .ok_or_else(|| archive_read_error(path, password_required()))?;
            archive
                .prepare_protected(encrypted, cancel)
                .map_err(|error| archive_read_error(path, error))?;
            archive
                .protected
                .validate()
                .map_err(|error| archive_read_error(path, error))?;
            // Report an ordinary wrong password before asking about conflicts.
            let mut input = EntryReader {
                file: &archive.file,
                cancel,
            };
            drop(
                archive
                    .protected
                    .open(first, &mut input, password)
                    .map_err(|error| archive_read_error(path, error))?,
            );
        }
        archive.verify_source()?;
        Ok((archive, trees))
    }

    pub(crate) fn verify_source(&self) -> Result<(), TransferError> {
        #[cfg(target_os = "macos")]
        self.mount.recheck()?;
        if self.source_matches().map_err(|source| {
            AccessError::new(&self.location(), AccessAction::ReadMetadata, source)
        })? {
            Ok(())
        } else {
            Err(ArchiveError::SourceChanged(self.location()).into())
        }
    }

    pub(super) fn source_matches(&self) -> io::Result<bool> {
        Ok(
            fs_ops::source_metadata_matches(&self.stamp, &self.file.metadata()?)?
                && fs_ops::source_metadata_matches(&self.stamp, &fs::metadata(&self.path)?)?,
        )
    }

    pub(crate) fn is_source_file(&self, target: &Path, metadata: &Metadata) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let _ = target;
            metadata.dev() == self.stamp.dev() && metadata.ino() == self.stamp.ino()
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            fs::canonicalize(&self.path)
                .is_ok_and(|path| fs::canonicalize(target).is_ok_and(|target| target == path))
        }
    }

    pub(crate) fn copy_entry(
        &mut self,
        entry: &SelectedEntry,
        path: &Path,
        output: &mut impl Write,
        progress: &mut Progress,
    ) -> io::Result<()> {
        self.consume(entry.contents, entry.size, path, Some(output), progress)
    }

    pub(super) fn check_payload(
        &self,
        contents: Contents,
        size: u64,
        path: &Path,
        progress: &mut Progress,
    ) -> io::Result<()> {
        self.consume(contents, size, path, None, progress)
    }

    fn consume(
        &self,
        contents: Contents,
        size: u64,
        path: &Path,
        output: Option<&mut dyn Write>,
        progress: &mut Progress,
    ) -> io::Result<()> {
        let cancel = progress.cancellation();
        let mut file = &self.file;
        match contents {
            Contents::Tar { offset } => {
                file.seek(SeekFrom::Start(offset))?;
                let mut input = file.take(size);
                copy_exact(path, size, &mut input, output, progress)
            }
            Contents::Zip {
                offset,
                length,
                crc,
                deflate,
            } => {
                file.seek(SeekFrom::Start(offset))?;
                let input = file.take(length);
                if deflate {
                    let decoder = DeflateReader {
                        input: BufReader::with_capacity(64 * 1024, input),
                        codec: Decompress::new(false),
                        length,
                        cancel,
                        finished: false,
                    };
                    copy_zip(path, size, crc, decoder, output, progress)
                } else {
                    copy_zip(path, size, crc, input, output, progress)
                }
            }
            Contents::Encrypted(member) => {
                let password = self.password.ok_or_else(password_required)?;
                let mut reader = EntryReader {
                    file: &self.file,
                    cancel: &cancel,
                };
                let mut input = self.protected.open(member, &mut reader, password)?;
                // Read through EOF in addition to enforcing the planned size.
                // In AES ZIP, the reader authenticates the data at EOF.
                copy_exact(path, size, &mut input, output, progress).map_err(|error| {
                    if error.kind() == io::ErrorKind::InvalidData {
                        ArchiveIssue::InvalidZipPasswordOrData.with_source(error)
                    } else {
                        error
                    }
                })
            }
            _ => Err(ArchiveIssue::InvalidEntry.io_error()),
        }
    }
}

struct Selection<'a> {
    index: &'a Index,
    source: &'a Location,
    cancel: &'a Cancellation,
    budget: &'a mut u64,
    entries: Vec<SelectedEntry>,
}

impl Selection<'_> {
    fn visit(&mut self, relative: PathBuf, member: Member) -> Result<(), TransferError> {
        self.cancel.check()?;
        let path = self.source.path().join(&relative);
        let location = || self.source.with_path(path.clone());
        let invalid_path = || ArchiveError::EntryPath {
            entry: location(),
            source: ArchiveMemberError::InvalidPath,
        };
        if !path
            .components()
            .all(|part| matches!(part, Component::Normal(name) if safe_component(name)))
        {
            return Err(invalid_path().into());
        }
        let size = match member.contents {
            Contents::Directory => 0,
            Contents::Zip {
                length,
                deflate: false,
                ..
            } if length != member.size => {
                return Err(AccessError::read(&location(), bad_data()).into());
            }
            Contents::Tar { offset } if offset.checked_add(member.size).is_none() => {
                return Err(AccessError::read(&location(), bad_data()).into());
            }
            Contents::Tar { .. } | Contents::Zip { .. } | Contents::Encrypted(_) => member.size,
            contents => {
                let reason = match contents {
                    Contents::UnsupportedEncryption => {
                        ArchiveIssue::UnsupportedZipEncryptionOrCompression
                    }
                    Contents::UnsupportedCompression => ArchiveIssue::UnsupportedZipCompression,
                    _ => ArchiveIssue::UnsupportedExtractionEntry,
                };
                return Err(AccessError::read(&location(), reason.io_error()).into());
            }
        };
        *self.budget = self
            .budget
            .checked_sub(size)
            .ok_or_else(|| ArchiveError::ExtractionLimit(location()))?;
        if member.is_dir {
            self.entries.push(SelectedEntry {
                relative: relative.clone(),
                size,
                modified: member.modified,
                contents: member.contents,
            });
            let children = self.index.folders.get(&path).ok_or_else(invalid_path)?;
            for (name, member) in children {
                self.visit(relative.join(name), *member)?;
            }
        } else {
            self.entries.push(SelectedEntry {
                relative,
                size,
                modified: member.modified,
                contents: member.contents,
            });
        }
        Ok(())
    }
}

fn safe_component(name: &std::ffi::OsStr) -> bool {
    // Always enforce a single path component. In particular, a component may
    // not turn into a prefix when joined on the host.
    matches!(Path::new(name).components().next(), Some(Component::Normal(part)) if part == name)
        && !name.as_encoded_bytes().contains(&b'\\')
}

pub(super) fn archive_read_error(path: &Path, source: io::Error) -> TransferError {
    ArchiveError::Read {
        archive: Location::Local(path.to_owned()),
        source,
    }
    .into()
}

fn bad_data() -> io::Error {
    ArchiveIssue::CorruptEntryData.io_error()
}

pub(super) fn copy_exact(
    path: &Path,
    size: u64,
    input: &mut impl Read,
    output: Option<&mut dyn Write>,
    progress: &mut Progress,
) -> io::Result<()> {
    let copied = if let Some(mut output) = output {
        progress.copy(
            path,
            Some(size),
            &mut input.by_ref().take(size),
            &mut output,
        )?
    } else {
        progress.read_stream(path, size, &mut *input, |reader| {
            io::copy(reader, &mut io::sink()).map(|_| ())
        })?
    };
    progress.check_cancelled()?;
    if copied != size || input.read(&mut [0])? != 0 {
        return Err(ArchiveIssue::EntrySizeMismatch.io_error());
    }
    Ok(())
}

fn copy_zip(
    path: &Path,
    size: u64,
    crc: u32,
    input: impl Read,
    output: Option<&mut dyn Write>,
    progress: &mut Progress,
) -> io::Result<()> {
    let mut input = CrcReader::new(input);
    copy_exact(path, size, &mut input, output, progress)?;
    if input.crc().sum() != crc {
        return Err(ArchiveIssue::ZipCrcMismatch.io_error());
    }
    Ok(())
}

struct DeflateReader<R> {
    input: BufReader<R>,
    codec: Decompress,
    length: u64,
    cancel: Cancellation,
    finished: bool,
}

impl<R: Read> Read for DeflateReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.finished {
            return Ok(0);
        }
        loop {
            self.cancel.check()?;
            let input = self.input.fill_buf()?;
            let before = self.codec.total_in();
            let written = self.codec.total_out();
            let status = self
                .codec
                .decompress(input, buffer, FlushDecompress::None)
                .map_err(|error| ArchiveIssue::CorruptEntryData.with_source(error))?;
            let consumed = (self.codec.total_in() - before) as usize;
            let produced = (self.codec.total_out() - written) as usize;
            self.input.consume(consumed);
            if status == Status::StreamEnd {
                if self.codec.total_in() != self.length {
                    return Err(bad_data());
                }
                self.finished = true;
                return Ok(produced);
            }
            if produced != 0 {
                return Ok(produced);
            }
            if consumed == 0 {
                return Err(bad_data());
            }
        }
    }
}
