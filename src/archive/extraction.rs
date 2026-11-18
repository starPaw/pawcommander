use super::protected_zip::{password_required, EntryReader, ProtectedZip};
use super::{invalid, open_regular, Contents, Format, Index, Member};
use crate::{
    fs_ops,
    i18n::text,
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
    pub(crate) fn prepare(
        sources: &[Location],
        password: Option<&'a str>,
        cancel: &Cancellation,
    ) -> Result<(Self, Vec<SelectedTree>), TransferError> {
        let first = sources.first().ok_or_else(selection_required)?;
        let path = first.archive_source().ok_or_else(selection_required)?;
        let parent = first.path().parent().ok_or_else(selection_required)?;
        let format = Format::from_path(path).ok_or_else(selection_required)?;
        if sources.iter().any(|source| {
            source.archive_source() != Some(path) || source.path().parent() != Some(parent)
        }) {
            return Err(selection_required().into());
        }
        cancel.check()?;
        let (file, stamp) = open_regular(path).map_err(|error| archive_read_error(path, error))?;
        let parsed = Index::read(&file, stamp.len(), format, &|| cancel.is_requested());
        cancel.check()?;
        let index = parsed.map_err(|error| archive_read_error(path, error))?;
        let mut archive = Self {
            path: path.into(),
            file,
            stamp,
            protected: ProtectedZip::default(),
            password: password.filter(|password| !password.is_empty()),
        };
        archive.verify_source()?;

        let mut budget = MAX_OUTPUT_BYTES;
        let mut seen = BTreeSet::new();
        let mut trees = Vec::new();
        let folder = index.folders.get(parent).ok_or_else(selection_required)?;
        for source in sources {
            cancel.check()?;
            let name = source.path().file_name().ok_or_else(selection_required)?;
            if !seen.insert(name) {
                continue;
            }
            let member = folder.get(name).ok_or_else(|| {
                crate::tr_format!(
                    "В архиве больше нет записи {source}",
                    "Archive entry no longer exists: {source}"
                )
            })?;
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
        let encrypted = trees
            .iter()
            .flat_map(|tree| &tree.entries)
            .filter_map(|entry| match entry.contents {
                Contents::Encrypted(member) => Some((member, entry.size)),
                _ => None,
            });
        let prepared = ProtectedZip::read(
            &archive.file,
            archive.stamp.len(),
            encrypted,
            archive.password,
            cancel,
        );
        cancel.check()?;
        archive.protected = prepared.map_err(|error| archive_read_error(path, error))?;
        archive.verify_source()?;
        Ok((archive, trees))
    }

    pub(crate) fn verify_source(&self) -> Result<(), TransferError> {
        let unchanged = self.file.metadata().ok().is_some_and(|current| {
            fs_ops::source_metadata_matches(&self.stamp, &current).unwrap_or(false)
        }) && fs::metadata(&self.path).ok().is_some_and(|current| {
            fs_ops::source_metadata_matches(&self.stamp, &current).unwrap_or(false)
        });
        if unchanged {
            Ok(())
        } else {
            Err(crate::tr_format!(
                "Архив изменился во время распаковки: {}",
                "The archive changed during extraction: {}",
                self.path.display()
            )
            .into())
        }
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
        let cancel = progress.cancellation();
        match entry.contents {
            Contents::Tar { offset } => {
                self.file.seek(SeekFrom::Start(offset))?;
                let mut input = (&mut self.file).take(entry.size);
                copy_exact(path, entry.size, &mut input, output, progress)
            }
            Contents::Zip {
                offset,
                length,
                crc,
                deflate,
            } => {
                self.file.seek(SeekFrom::Start(offset))?;
                let input = (&mut self.file).take(length);
                if deflate {
                    let decoder = DeflateReader {
                        input: BufReader::with_capacity(64 * 1024, input),
                        codec: Decompress::new(false),
                        length,
                        cancel,
                        finished: false,
                    };
                    copy_zip(path, entry.size, crc, decoder, output, progress)
                } else {
                    copy_zip(path, entry.size, crc, input, output, progress)
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
                copy_exact(path, entry.size, &mut input, output, progress).map_err(|error| {
                    if error.kind() == io::ErrorKind::InvalidData {
                        invalid("Неверный пароль или повреждены данные ZIP")
                    } else {
                        error
                    }
                })
            }
            _ => Err(invalid("Недопустимая запись архива")),
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
        for part in path.components() {
            let Component::Normal(name) = part else {
                return Err(super::invalid_path().into());
            };
            if !safe_component(name) {
                return Err(super::invalid_path().into());
            }
        }
        let size = match member.contents {
            Contents::Directory => 0,
            Contents::Zip {
                length,
                deflate: false,
                ..
            } if length != member.size => {
                return Err(bad_data().into());
            }
            Contents::Tar { offset } if offset.checked_add(member.size).is_none() => {
                return Err(bad_data().into());
            }
            Contents::Tar { .. } | Contents::Zip { .. } | Contents::Encrypted(_) => member.size,
            contents => {
                let reason = text(match contents {
                    Contents::UnsupportedEncryption => {
                        "Шифрование или сжатие ZIP не поддерживается"
                    }
                    Contents::UnsupportedCompression => "Метод сжатия ZIP не поддерживается",
                    _ => "Распаковка поддерживает только обычные файлы и папки",
                });
                return Err(format!("{}: {reason}", self.source.with_path(path)).into());
            }
        };
        *self.budget = self
            .budget
            .checked_sub(size)
            .ok_or_else(|| text("Выбрано больше 32 ГиБ для распаковки"))?;
        if member.is_dir {
            self.entries.push(SelectedEntry {
                relative: relative.clone(),
                size,
                modified: member.modified,
                contents: member.contents,
            });
            let children = self
                .index
                .folders
                .get(&path)
                .ok_or_else(super::invalid_path)?;
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

fn selection_required() -> &'static str {
    text("Выберите записи из одной папки архива")
}

fn archive_read_error(path: &Path, error: io::Error) -> TransferError {
    TransferError::io(error, |error| {
        crate::tr_format!(
            "Не удалось прочитать архив «{}»: {error}",
            "Could not read archive “{}”: {error}",
            path.display()
        )
    })
}

fn bad_data() -> io::Error {
    invalid("Повреждены данные записи архива")
}

fn copy_exact(
    path: &Path,
    size: u64,
    input: &mut impl Read,
    output: &mut impl Write,
    progress: &mut Progress,
) -> io::Result<()> {
    let copied = progress.copy(path, Some(size), &mut input.by_ref().take(size), output)?;
    progress.check_cancelled()?;
    if copied != size || input.read(&mut [0])? != 0 {
        return Err(bad_data());
    }
    Ok(())
}

fn copy_zip(
    path: &Path,
    size: u64,
    crc: u32,
    input: impl Read,
    output: &mut impl Write,
    progress: &mut Progress,
) -> io::Result<()> {
    let mut input = CrcReader::new(input);
    copy_exact(path, size, &mut input, output, progress)?;
    if input.crc().sum() != crc {
        return Err(bad_data());
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
                .map_err(|_| bad_data())?;
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
