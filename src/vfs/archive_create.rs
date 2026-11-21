use super::location::{validate_name, Location};
use super::staged_file::StagedFile;
use super::transfer::{
    canonical, changed_source, entry_metadata, rename_for_publication, source_children,
    with_staging_typed, EntryMetadata, SourceReader, TransferSourceEntry,
};
use super::undo::Parent;
use crate::archive::{ArchiveWriter, Attributes, CreationLimits, Format};
use crate::operations::{Cancellation, Progress, TransferError};
use crate::result_history::Outcome;
use std::{collections::BTreeSet, io, path::Path, time::UNIX_EPOCH};

pub(crate) fn archive_file_name(name: &str, format: Format) -> Result<String, String> {
    let name = name.trim();
    validate_name(name)
        .map_err(|_| crate::i18n::text!("Введите допустимое имя архива").to_owned())?;
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
    password: Option<&str>,
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
    let password = password.filter(|password| !password.is_empty());
    let result = write_archive(
        sources,
        directory,
        &name,
        format,
        password,
        progress,
        &mut published,
    );
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
            "Archive created: {target}",
            target = target
        )),
        Err(TransferError::Cancelled) => {
            progress.results.cancelled = true;
            Ok(crate::i18n::text!("Создание архива остановлено").into())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn write_archive(
    sources: &[Location],
    directory: &Location,
    name: &str,
    format: Format,
    password: Option<&str>,
    progress: &mut Progress,
    published: &mut bool,
) -> Result<(), TransferError> {
    if directory.is_archive() || sources.iter().any(Location::is_archive) {
        return Err(crate::archive::readonly().into());
    }
    if format == Format::Tar && password.is_some() {
        return Err(crate::i18n::text!("Пароль можно задать только для ZIP").into());
    }
    if sources.is_empty()
        || sources
            .iter()
            .any(|source| source.parent().as_ref() != Some(directory))
    {
        return Err(crate::i18n::text!("Выберите файлы или папки из текущей папки").into());
    }
    let cancel = progress.cancellation();
    cancel.check()?;
    let parent = Parent::capture(directory)?;
    let target = parent.location.join(name);
    ensure_absent(&target)?;

    let mut plan = ArchivePlan::new(format, password.is_some());
    plan.capture_sources(sources, &parent.location, Path::new(""), progress, &cancel)?;

    with_staging_typed(&target, Some(&cancel), |staging, _| {
        let payload = staging.join("payload");
        let output = StagedFile::open(&payload, cancel.clone())?;
        let mut archive = ArchiveWriter::new(format, password, output)?;
        plan.append(&mut archive, progress, &cancel)?;
        progress.item(target.path());
        archive.finish()?.close()?;
        plan.verify(&cancel)?;
        parent.verify()?;
        if canonical(directory)? != parent.location.path() {
            return Err(changed_source(directory).into());
        }
        ensure_absent(&target)?;
        cancel.check()?;
        rename_for_publication(&payload, &target).map_err(|error| match error {
            TransferError::PublicationUnconfirmed(error) => {
                TransferError::PublicationUnconfirmed(crate::tr_format!(
                    "The server did not confirm publication of {target}: {error}",
                    error = error,
                    target = target
                ))
            }
            error => error,
        })?;
        *published = true;
        Ok(())
    })
}

fn invalid_name() -> &'static str {
    crate::i18n::text!("Для создания архива нужны имена файлов в UTF-8")
}

fn ensure_absent(target: &Location) -> Result<(), TransferError> {
    if entry_metadata(target)?.is_some() {
        return Err(
            crate::tr_format!("File or folder already exists: {target}", target = target).into(),
        );
    }
    Ok(())
}

pub(super) struct ArchivePlan {
    format: Format,
    protected: bool,
    limits: CreationLimits,
    members: Vec<ArchiveSource>,
}

impl ArchivePlan {
    pub(super) fn new(format: Format, protected: bool) -> Self {
        Self {
            format,
            protected,
            limits: CreationLimits::default(),
            members: Vec::new(),
        }
    }

    pub(super) fn capture_sources(
        &mut self,
        sources: &[Location],
        directory: &Location,
        prefix: &Path,
        progress: &mut Progress,
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        let mut selected = BTreeSet::new();
        for source in sources {
            let name = source
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(invalid_name)?;
            if selected.insert(name) {
                let path = prefix.join(name);
                self.capture(
                    directory.join(name),
                    path.to_str().ok_or_else(invalid_name)?.to_owned(),
                    progress,
                    cancel,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn names(&self) -> impl Iterator<Item = (&Path, bool)> + Clone {
        self.members
            .iter()
            .map(|member| (Path::new(&member.name), member.attributes.directory))
    }

    pub(super) fn contains_file(&self, metadata: &EntryMetadata) -> bool {
        self.members
            .iter()
            .any(|member| member.entry.metadata.same_object(metadata))
    }

    pub(super) fn append(
        &self,
        archive: &mut ArchiveWriter<'_, StagedFile>,
        progress: &mut Progress,
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        for member in &self.members {
            member.append(archive, progress, cancel)?;
        }
        Ok(())
    }

    pub(super) fn verify(&self, cancel: &Cancellation) -> Result<(), TransferError> {
        for member in &self.members {
            member.entry.verify(&member.source, cancel, true)?;
        }
        Ok(())
    }

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
        self.limits
            .add(self.format, &name, attributes.directory, self.protected)?;
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
        archive: &mut ArchiveWriter<'_, StagedFile>,
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
            "Only regular files and folders can be added to an archive: {source}",
            source = source
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
                        "The server did not report the source file size: {source}",
                        source = source
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Index;
    use std::fs;
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
                None,
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
            None,
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
            None,
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
        assert!(create_archive(
            &[source],
            &root,
            "archive",
            Format::Zip,
            None,
            &mut progress
        )
        .is_err());
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
