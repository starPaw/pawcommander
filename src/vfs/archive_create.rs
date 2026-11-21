use super::location::{validate_name, Location};
use super::staged_file::StagedFile;
use super::transfer::{
    canonical, entry_metadata, rename_for_publication, source_children, with_staging_typed,
    EntryMetadata, SourceReader, TransferSourceEntry,
};
use super::undo::Parent;
use crate::archive::{ArchiveWriter, Attributes, CreationLimits, Format};
use crate::file_error::{
    AccessAction, AccessError, ArchiveError, MutationError, PathError, PermissionError,
};
use crate::operations::{Cancellation, Progress, TransferError};
use crate::result_history::Outcome;
use std::{collections::BTreeSet, io, path::Path, time::UNIX_EPOCH};

pub(crate) fn archive_file_name(name: &str, format: Format) -> Result<String, PathError> {
    let name = name.trim();
    validate_name(name)?;
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
) -> Result<Location, TransferError> {
    let name = archive_file_name(name, format).map_err(ArchiveError::InvalidName)?;
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
    let outcome = match &result {
        Ok(()) => Outcome::Completed,
        Err(_) if published => Outcome::CompletedWithWarning,
        Err(TransferError::Cancelled) => Outcome::Cancelled,
        Err(TransferError::PublicationUnconfirmed(_)) => Outcome::PublicationUnconfirmed,
        Err(TransferError::Failed(_)) => Outcome::Failed,
    };
    for source in sources {
        progress
            .results
            .record_with_error(source, outcome, result.as_ref().err());
    }
    if matches!(result, Err(TransferError::Cancelled)) {
        progress.results.cancelled = true;
    }
    result.map(|()| target)
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
    if let Some(location) = std::iter::once(directory)
        .chain(sources)
        .find(|source| source.is_archive())
    {
        return Err(MutationError::ReadOnlyArchive(location.clone()).into());
    }
    if format == Format::Tar && password.is_some() {
        return Err(ArchiveError::PasswordRequiresZip.into());
    }
    if sources.is_empty()
        || sources
            .iter()
            .any(|source| source.parent().as_ref() != Some(directory))
    {
        return Err(ArchiveError::SelectCurrentFolder.into());
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
        let mut archive = ArchiveWriter::new(format, password, output)
            .map_err(|error| AccessError::write(&target, error))?;
        plan.append(&target, &mut archive, progress, &cancel)?;
        progress.item(target.path());
        archive
            .finish()
            .map_err(|error| AccessError::write(&target, error))?
            .close()?;
        plan.verify(&cancel)?;
        parent.verify()?;
        if canonical(directory)? != parent.location.path() {
            return Err(MutationError::SourceChanged(directory.clone()).into());
        }
        ensure_absent(&target)?;
        cancel.check()?;
        rename_for_publication(&payload, &target)?;
        *published = true;
        Ok(())
    })
}

fn ensure_absent(target: &Location) -> Result<(), TransferError> {
    if entry_metadata(target)?.is_some() {
        return Err(MutationError::DestinationExists(target.clone()).into());
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
                .ok_or_else(|| ArchiveError::EntryEncoding(source.clone()))?;
            if selected.insert(name) {
                let path = prefix.join(name);
                self.capture(
                    directory.join(name),
                    path.to_str()
                        .ok_or_else(|| ArchiveError::EntryEncoding(source.clone()))?
                        .to_owned(),
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
        target: &Location,
        archive: &mut ArchiveWriter<'_, StagedFile>,
        progress: &mut Progress,
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        for member in &self.members {
            member.append(target, archive, progress, cancel)?;
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
        let metadata =
            entry_metadata(&source)?.ok_or_else(|| MutationError::SourceChanged(source.clone()))?;
        let attributes = attributes(&source, &metadata)?;
        self.limits
            .add(self.format, &name, attributes.directory, self.protected)
            .map_err(|error| ArchiveError::EntryPath {
                entry: source.clone(),
                source: error,
            })?;
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
                let location = source.join(&child);
                let component = child
                    .to_str()
                    .ok_or_else(|| ArchiveError::EntryEncoding(location.clone()))?;
                self.capture(location, format!("{name}/{component}"), progress, cancel)?;
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
        target: &Location,
        archive: &mut ArchiveWriter<'_, StagedFile>,
        progress: &mut Progress,
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        self.entry.verify(&self.source, cancel, true)?;
        let add_entry_error = |source| {
            TransferError::from(ArchiveError::AddEntry {
                archive: target.clone(),
                entry: self.source.clone(),
                source,
            })
        };
        if self.attributes.directory {
            progress.item(self.source.path());
            return archive
                .append(&self.name, &self.attributes, io::empty())
                .map_err(add_entry_error);
        }
        let mut input = SourceReader::open(&self.source)?;
        self.entry
            .verify_metadata(&self.source, &input.metadata(&self.source)?)?;
        let expected = self.attributes.size;
        let count = progress
            .read_stream(self.source.path(), expected, &mut input, |reader| {
                archive.append(&self.name, &self.attributes, reader)
            })
            .map_err(add_entry_error)?;
        if count != expected {
            return Err(MutationError::SourceChanged(self.source.clone()).into());
        }
        self.entry
            .verify_metadata(&self.source, &input.metadata(&self.source)?)
    }
}

fn attributes(source: &Location, metadata: &EntryMetadata) -> Result<Attributes, TransferError> {
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
                    .modified()
                    .map_err(|error| AccessError::new(source, AccessAction::ReadMetadata, error))?
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|time| time.as_secs()),
            })
        }
        EntryMetadata::Remote(stat) if stat.is_file() || stat.is_dir() => Ok(Attributes {
            directory: stat.is_dir(),
            size: if stat.is_file() {
                stat.size
                    .ok_or_else(|| ArchiveError::MissingEntrySize(source.clone()))?
            } else {
                0
            },
            mode: stat
                .perm
                .ok_or_else(|| PermissionError::MissingMode(source.clone()))?
                & 0o777,
            modified: stat.mtime,
        }),
        _ => Err(ArchiveError::UnsupportedEntry(source.clone()).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Index;
    use crate::file_error::Failure;
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
            let output = create_archive(
                std::slice::from_ref(&folder),
                &root,
                "result",
                format,
                None,
                &mut progress,
            )
            .unwrap();
            assert_eq!(output, root.join(format!("result.{}", format.extension())));
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
        let result = create_archive(
            std::slice::from_ref(&source),
            &root,
            "cancelled",
            Format::Tar,
            None,
            &mut progress,
        );
        assert!(matches!(result, Err(TransferError::Cancelled)));
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
        let error = create_archive(
            std::slice::from_ref(&source),
            &root,
            "occupied",
            Format::Tar,
            None,
            &mut progress,
        )
        .unwrap_err();
        assert!(
            matches!(
                &error,
                TransferError::Failed(Failure::Mutation(reason))
                    if matches!(reason.as_ref(), MutationError::DestinationExists(location) if location == &target)
            ),
            "{error:?}"
        );
        assert_eq!(progress.results.items[0].outcome, Outcome::Failed);
        assert_eq!(fs::read(target.path()).unwrap(), b"other writer");
        assert_eq!(fs::read(source.path()).unwrap(), data);
        assert!(staging(temp.path()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_rejected_without_publishing_or_reading_the_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = Location::Local(temp.path().canonicalize().unwrap());
        let source = root.join("Folder");
        fs::create_dir(source.path()).unwrap();
        let outside = root.join("outside.txt");
        fs::write(outside.path(), DATA).unwrap();
        let link = source.join("external-link");
        std::os::unix::fs::symlink(outside.path(), link.path()).unwrap();
        let mut progress = Progress::default();
        let error = create_archive(
            &[source],
            &root,
            "archive",
            Format::Zip,
            None,
            &mut progress,
        )
        .unwrap_err();
        assert!(
            matches!(
                &error,
                TransferError::Failed(Failure::Archive(reason))
                    if matches!(reason.as_ref(), ArchiveError::UnsupportedEntry(location) if location == &link)
            ),
            "{error:?}"
        );
        assert!(!root.join("archive.zip").path().exists());
        assert!(staging(temp.path()).is_none());
        assert_eq!(fs::read(outside.path()).unwrap(), DATA);
        assert_eq!(fs::read_link(link.path()).unwrap(), outside.path());
    }

    fn large_fixture(directory: &Path) -> (Location, Location, Vec<u8>) {
        let root = Location::Local(directory.canonicalize().unwrap());
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
