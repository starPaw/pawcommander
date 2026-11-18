use super::location::{validate_name, Location};
use super::sftp::remote;
use super::transfer::{
    canonical, changed_source, entry_metadata, rename_for_publication, source_children,
    with_staging_typed, EntryMetadata, SourceReader, TransferSourceEntry,
};
use super::undo::Parent;
use crate::archive::{ArchiveWriter, Attributes, CreationLimits, Format};
use crate::operations::{Cancellation, Progress, TransferError};
use crate::result_history::Outcome;
use ssh2::{OpenFlags, OpenType};
use std::{
    collections::BTreeSet,
    fs,
    io::{self, BufWriter, Write},
    path::Path,
    time::UNIX_EPOCH,
};

pub(crate) fn archive_file_name(name: &str, format: Format) -> Result<String, String> {
    let name = name.trim();
    validate_name(name)
        .map_err(|_| crate::i18n::text("Введите допустимое имя архива").to_owned())?;
    if Format::from_path(Path::new(name)) == Some(format) {
        Ok(name.to_owned())
    } else {
        Ok(format!("{name}.{}", format.extension()))
    }
}

pub(crate) fn create_archive(
    sources: &[Location],
    directory: &Location,
    name: &str,
    format: Format,
    progress: &mut Progress,
) -> Result<String, String> {
    let name = archive_file_name(name, format)?;
    let target = directory.join(&name);
    progress.results.plan(
        sources
            .iter()
            .map(|source| (source.clone(), Some(target.display()))),
    );
    let mut published = false;
    let result = write_archive(sources, directory, &name, format, progress, &mut published);
    let (outcome, reason) = match &result {
        Ok(()) => (Outcome::Completed, None),
        Err(error) => (
            match error {
                _ if published => Outcome::CompletedWithWarning,
                TransferError::Cancelled => Outcome::Cancelled,
                TransferError::PublicationUnconfirmed(_) => Outcome::PublicationUnconfirmed,
                TransferError::Failed(_) => Outcome::Failed,
            },
            Some(error.to_string()),
        ),
    };
    for source in sources {
        progress
            .results
            .record(source, None, outcome, reason.clone());
    }
    match result {
        Ok(()) => Ok(crate::tr_format!(
            "Архив создан: {target}",
            "Archive created: {target}"
        )),
        Err(TransferError::Cancelled) => {
            progress.results.cancelled = true;
            Ok(crate::i18n::text("Создание архива остановлено").into())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn write_archive(
    sources: &[Location],
    directory: &Location,
    name: &str,
    format: Format,
    progress: &mut Progress,
    published: &mut bool,
) -> Result<(), TransferError> {
    if directory.is_archive() || sources.iter().any(Location::is_archive) {
        return Err(crate::archive::readonly().into());
    }
    if sources.is_empty()
        || sources
            .iter()
            .any(|source| source.parent().as_ref() != Some(directory))
    {
        return Err(crate::i18n::text("Выберите файлы или папки из текущей папки").into());
    }
    let cancel = progress.cancellation();
    cancel.check()?;
    let parent = Parent::capture(directory)?;
    let target = parent.location.join(name);
    ensure_absent(&target)?;

    let mut plan = ArchivePlan {
        format,
        limits: CreationLimits::default(),
        members: Vec::new(),
    };
    let mut selected = BTreeSet::new();
    for source in sources {
        let name = source
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(invalid_name)?;
        if selected.insert(name) {
            plan.capture(
                parent.location.join(name),
                name.to_owned(),
                progress,
                &cancel,
            )?;
        }
    }

    with_staging_typed(&target, Some(&cancel), |staging, _| {
        let payload = staging.join("payload");
        let output = ArchiveOutput::open(&payload, cancel.clone())?;
        let mut archive = ArchiveWriter::new(format, output);
        for member in &plan.members {
            member.append(&mut archive, progress, &cancel)?;
        }
        progress.item(target.path());
        archive.finish()?.close()?;
        for member in &plan.members {
            member.entry.verify(&member.source, &cancel, true)?;
        }
        parent.verify()?;
        if canonical(directory)? != parent.location.path() {
            return Err(changed_source(directory).into());
        }
        ensure_absent(&target)?;
        cancel.check()?;
        rename_for_publication(&payload, &target).map_err(|error| match error {
            TransferError::PublicationUnconfirmed(error) => {
                TransferError::PublicationUnconfirmed(crate::tr_format!(
                    "Сервер не подтвердил сохранение архива {target}: {error}",
                    "The server did not confirm publication of {target}: {error}"
                ))
            }
            error => error,
        })?;
        *published = true;
        Ok(())
    })
}

fn invalid_name() -> &'static str {
    crate::i18n::text("Для создания архива нужны имена файлов в UTF-8")
}

fn ensure_absent(target: &Location) -> Result<(), TransferError> {
    if entry_metadata(target)?.is_some() {
        return Err(crate::tr_format!(
            "Файл или папка уже существует: {target}",
            "File or folder already exists: {target}"
        )
        .into());
    }
    Ok(())
}

struct ArchivePlan {
    format: Format,
    limits: CreationLimits,
    members: Vec<ArchiveSource>,
}

impl ArchivePlan {
    fn capture(
        &mut self,
        source: Location,
        name: String,
        progress: &mut Progress,
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        cancel.check()?;
        progress.item(source.path());
        let metadata = entry_metadata(&source)?.ok_or_else(|| changed_source(&source))?;
        let attributes = attributes(&source, &metadata)?;
        self.limits.add(self.format, &name, attributes.directory)?;
        let children = if attributes.directory {
            Some(source_children(&source, cancel, true)?)
        } else {
            None
        };
        self.members.push(ArchiveSource {
            source: source.clone(),
            name: name.clone(),
            entry: TransferSourceEntry {
                metadata,
                children: children.clone(),
            },
            attributes,
        });
        if let Some(children) = children {
            for child in children {
                let component = child.to_str().ok_or_else(invalid_name)?;
                self.capture(
                    source.join(&child),
                    format!("{name}/{component}"),
                    progress,
                    cancel,
                )?;
            }
        }
        Ok(())
    }
}

struct ArchiveSource {
    source: Location,
    name: String,
    entry: TransferSourceEntry,
    attributes: Attributes,
}

impl ArchiveSource {
    fn append(
        &self,
        archive: &mut ArchiveWriter<ArchiveOutput>,
        progress: &mut Progress,
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        self.entry.verify(&self.source, cancel, true)?;
        let context = |error| format!("{}: {error}", self.source);
        if self.attributes.directory {
            progress.item(self.source.path());
            return archive
                .append(&self.name, &self.attributes, io::empty())
                .map_err(|error| TransferError::io(error, context));
        }
        let mut input = SourceReader::open(&self.source)?;
        self.entry
            .verify_metadata(&self.source, &input.metadata()?)?;
        let expected = self.attributes.size;
        let count = progress
            .read_stream(self.source.path(), expected, &mut input, |reader| {
                archive.append(&self.name, &self.attributes, reader)
            })
            .map_err(|error| TransferError::io(error, context))?;
        if count != expected {
            return Err(changed_source(&self.source).into());
        }
        self.entry
            .verify_metadata(&self.source, &input.metadata()?)?;
        Ok(())
    }
}

fn attributes(source: &Location, metadata: &EntryMetadata) -> Result<Attributes, TransferError> {
    let unsupported = || {
        crate::tr_format!(
            "Архив поддерживает только обычные файлы и папки: {source}",
            "Only regular files and folders can be added to an archive: {source}"
        )
    };
    match metadata {
        EntryMetadata::Local(meta) if meta.is_file() || meta.is_dir() => {
            #[cfg(unix)]
            let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions());
            #[cfg(not(unix))]
            let mode = if meta.is_dir() { 0o755 } else { 0o644 };
            Ok(Attributes {
                directory: meta.is_dir(),
                size: if meta.is_file() { meta.len() } else { 0 },
                mode: mode & 0o777,
                modified: meta
                    .modified()?
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|time| time.as_secs()),
            })
        }
        EntryMetadata::Remote(stat) if stat.is_file() || stat.is_dir() => Ok(Attributes {
            directory: stat.is_dir(),
            size: if stat.is_file() {
                stat.size.ok_or_else(|| {
                    crate::tr_format!(
                        "Сервер не сообщил размер исходного файла: {source}",
                        "The server did not report the source file size: {source}"
                    )
                })?
            } else {
                0
            },
            mode: stat.perm.ok_or_else(unsupported)? & 0o777,
            modified: stat.mtime,
        }),
        _ => Err(unsupported().into()),
    }
}

enum ArchiveFile {
    Local(fs::File),
    Remote(ssh2::File),
}

impl Write for ArchiveFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Local(file) => file.write(bytes),
            Self::Remote(file) => file.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Local(file) => file.flush(),
            Self::Remote(file) => file.flush(),
        }
    }
}

struct ArchiveOutput {
    file: BufWriter<ArchiveFile>,
    cancel: Cancellation,
}

impl ArchiveOutput {
    fn open(payload: &Location, cancel: Cancellation) -> Result<Self, TransferError> {
        cancel.check()?;
        let file = if let Some(path) = payload.local() {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            ArchiveFile::Local(options.open(path)?)
        } else {
            let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUSIVE;
            ArchiveFile::Remote(
                remote(payload)?
                    .open_mode(payload.path(), flags, 0o600, OpenType::File)
                    .map_err(super::err)?,
            )
        };
        Ok(Self {
            file: BufWriter::with_capacity(64 * 1024, file),
            cancel,
        })
    }

    fn close(mut self) -> Result<(), TransferError> {
        self.flush()?;
        match self.file.into_inner().map_err(|error| error.into_error())? {
            ArchiveFile::Local(file) => file.sync_all()?,
            ArchiveFile::Remote(mut file) => file.close().map_err(super::err)?,
        }
        Ok(())
    }
}

impl Write for ArchiveOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.cancel.check()?;
        self.file.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.cancel.check()?;
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Index;
    use std::io::Read;
    use std::path::PathBuf;

    const DATA: &[u8] = b"An archived file retains its exact content.\n";

    #[test]
    fn both_formats_keep_nested_files_and_empty_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = Location::Local(temp.path().into());
        let folder = root.join("Folder");
        fs::create_dir_all(folder.join("Empty").path()).unwrap();
        let long_name = format!("{}-данные.txt", "x".repeat(101));
        let file = folder.join(&long_name);
        fs::write(file.path(), DATA).unwrap();
        let mut progress = Progress::default();
        for format in [Format::Zip, Format::Tar] {
            create_archive(
                std::slice::from_ref(&folder),
                &root,
                "result",
                format,
                &mut progress,
            )
            .unwrap();
            let output = root.join(format!("result.{}", format.extension()));
            assert_eq!(progress.results.items[0].outcome, Outcome::Completed);
            let index = Index::load(output.path(), &|| false).unwrap();
            let names = index
                .list(&output.browse_archive().unwrap().join("Folder"), &|| false)
                .unwrap();
            assert!(names
                .iter()
                .any(|entry| entry.name == "Empty" && entry.is_dir));
            assert!(names
                .iter()
                .any(|entry| entry.name == long_name && entry.size == DATA.len() as u64));
            let mut decoded = Vec::new();
            let input = fs::File::open(output.path()).unwrap();
            match format {
                Format::Zip => {
                    let mut buffer = vec![0; rawzip::RECOMMENDED_BUFFER_SIZE];
                    let zip = rawzip::ZipArchive::from_seekable(input, &mut buffer).unwrap();
                    let mut entries = zip.entries(&mut buffer);
                    while let Some(entry) = entries.next_entry().unwrap() {
                        if entry.file_path().as_bytes().ends_with(long_name.as_bytes()) {
                            assert_eq!(
                                entry.compression_method(),
                                rawzip::CompressionMethod::DEFLATE
                            );
                            let file = zip.get_entry(entry.wayfinder()).unwrap();
                            let decoder = flate2::read::DeflateDecoder::new(file.reader());
                            file.verifying_reader(decoder)
                                .read_to_end(&mut decoded)
                                .unwrap();
                        }
                    }
                }
                Format::Tar => {
                    for entry in tar::Archive::new(input).entries().unwrap() {
                        let mut entry = entry.unwrap();
                        if entry.path().unwrap() == Path::new("Folder").join(&long_name) {
                            entry.read_to_end(&mut decoded).unwrap();
                        }
                    }
                }
            }
            assert_eq!(decoded, DATA);
        }
        assert_eq!(fs::read(file.path()).unwrap(), DATA);
        assert!(staging(temp.path()).is_none());
    }

    #[test]
    fn cancelling_a_partially_written_archive_cleans_staging() {
        let temp = tempfile::tempdir().unwrap();
        let (root, source, data) = large_fixture(temp.path());
        let watch = root.path().to_owned();
        let cancel = Cancellation::at_checkpoint(move || partial_payload(&watch));
        let mut progress = Progress::with_cancellation(cancel.clone());
        create_archive(
            std::slice::from_ref(&source),
            &root,
            "cancelled",
            Format::Tar,
            &mut progress,
        )
        .unwrap();
        assert!(cancel.is_requested());
        assert!(progress.results.cancelled);
        assert_eq!(progress.results.items[0].outcome, Outcome::Cancelled);
        assert!(!root.join("cancelled.tar").path().exists());
        assert!(staging(temp.path()).is_none());
        assert_eq!(fs::read(source.path()).unwrap(), data);
    }

    #[test]
    fn a_destination_created_during_streaming_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let (root, source, data) = large_fixture(temp.path());
        let watch = root.path().to_owned();
        let target = root.join("occupied.tar");
        let target_path = target.path().to_owned();
        let cancel = Cancellation::at_checkpoint(move || {
            if !target_path.exists() && partial_payload(&watch) {
                fs::write(&target_path, b"other writer").unwrap();
            }
            false
        });
        let mut progress = Progress::with_cancellation(cancel);
        assert!(create_archive(
            std::slice::from_ref(&source),
            &root,
            "occupied",
            Format::Tar,
            &mut progress
        )
        .is_err());
        assert_eq!(progress.results.items[0].outcome, Outcome::Failed);
        assert_eq!(fs::read(target.path()).unwrap(), b"other writer");
        assert_eq!(fs::read(source.path()).unwrap(), data);
        assert!(staging(temp.path()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_rejected_without_publishing_or_reading_the_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = Location::Local(temp.path().into());
        let source = root.join("Folder");
        fs::create_dir(source.path()).unwrap();
        let outside = root.join("outside.txt");
        fs::write(outside.path(), DATA).unwrap();
        let link = source.join("external-link");
        std::os::unix::fs::symlink(outside.path(), link.path()).unwrap();
        let mut progress = Progress::default();
        assert!(create_archive(&[source], &root, "archive", Format::Zip, &mut progress).is_err());
        assert!(!root.join("archive.zip").path().exists());
        assert!(staging(temp.path()).is_none());
        assert_eq!(fs::read(outside.path()).unwrap(), DATA);
        assert_eq!(fs::read_link(link.path()).unwrap(), outside.path());
    }

    fn large_fixture(directory: &Path) -> (Location, Location, Vec<u8>) {
        let root = Location::Local(directory.into());
        let source = root.join("source.dat");
        let data = vec![0x5a; 1024 * 1024];
        fs::write(source.path(), &data).unwrap();
        (root, source, data)
    }

    fn staging(directory: &Path) -> Option<PathBuf> {
        fs::read_dir(directory)
            .unwrap()
            .map(Result::unwrap)
            .find_map(|item| {
                item.file_name()
                    .to_string_lossy()
                    .starts_with(".pawcommander-transfer-")
                    .then(|| item.path())
            })
    }

    fn partial_payload(directory: &Path) -> bool {
        staging(directory).is_some_and(|path| {
            fs::metadata(path.join("payload"))
                .is_ok_and(|meta| (1..1024 * 1024).contains(&meta.len()))
        })
    }
}
