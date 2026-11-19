mod manifest;
#[cfg(test)]
mod tests;
#[cfg(all(test, target_os = "macos"))]
pub(super) use tests::checksum_round_trip;

use super::{
    location::validate_name,
    staged_file::StagedFile,
    transfer::{
        canonical, entry_metadata, rename_for_publication, with_staging_typed, EntryMetadata,
        SourceReader, TransferSourceEntry,
    },
    undo::Parent,
    Location,
};
use crate::{
    i18n::text,
    operations::{Cancellation, Progress, TransferError},
    result_history::Outcome,
};
#[cfg(unix)]
use std::io;
use std::{io::Write, path::Path};

pub(crate) fn checksum_manifest_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    validate_name(name).map_err(|_| text("Введите допустимое имя манифеста").to_owned())?;
    Ok(name)
}

pub(crate) fn looks_like_checksum_manifest(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("sha256"))
        || path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("SHA256SUMS"))
}

pub(crate) fn create_checksum_manifest(
    directory: &Location,
    name: &str,
    sources: &[Location],
    progress: &mut Progress,
) -> Result<String, String> {
    let name = checksum_manifest_name(name)?;
    let target = directory.join(name);
    progress.results.plan([(target.clone(), None)]);
    progress.results.begin(&target);
    let mut published = false;
    match write_manifest(directory, name, sources, progress, &mut published) {
        Ok(()) => Ok(crate::tr_format!(
            "Манифест SHA-256 создан: {target}",
            "SHA-256 manifest created: {target}"
        )),
        Err(TransferError::Cancelled) => {
            progress.results.cancelled = true;
            Ok(text("Создание манифеста остановлено").into())
        }
        Err(error) => {
            let outcome = if published {
                Outcome::CompletedWithWarning
            } else if matches!(error, TransferError::PublicationUnconfirmed(_)) {
                Outcome::PublicationUnconfirmed
            } else {
                Outcome::Failed
            };
            let message = error.to_string();
            progress
                .results
                .record(&target, None, outcome, Some(message.clone()));
            Err(message)
        }
    }
}

fn write_manifest(
    directory: &Location,
    name: &str,
    sources: &[Location],
    progress: &mut Progress,
    published: &mut bool,
) -> Result<(), TransferError> {
    if sources.is_empty()
        || sources
            .iter()
            .any(|source| source.parent().as_ref() != Some(directory))
    {
        return Err(text("Выберите обычные файлы из текущей папки").into());
    }
    if sources.len() > manifest::MAX_ENTRIES {
        return Err(manifest::too_large());
    }
    let cancel = progress.cancellation();
    cancel.check()?;
    let scope = Directory::open(directory)?;
    let target = scope.parent.location.join(name);
    ensure_absent(&target)?;
    let mut body = Vec::new();
    for source in sources {
        cancel.check()?;
        let relative = Path::new(
            source
                .path()
                .file_name()
                .ok_or_else(|| text("Выберите обычные файлы из текущей папки"))?,
        );
        let digest = scope
            .digest(relative, progress, &cancel)
            .map_err(|error| file_error(source, error))?
            .ok_or_else(|| {
                crate::tr_format!("Файл отсутствует: {source}", "File is missing: {source}")
            })?;
        manifest::append(&mut body, relative, digest)?;
    }

    progress.item(target.path());
    with_staging_typed(&target, Some(&cancel), |staging, _| {
        let payload = staging.join("payload");
        let mut output = StagedFile::open(&payload, cancel.clone())?;
        for bytes in body.chunks(64 * 1024) {
            output.write_all(bytes)?;
        }
        output.close()?;
        scope.parent.verify()?;
        if canonical(directory)? != scope.parent.location.path() {
            return Err(text("Папка манифеста изменилась").into());
        }
        ensure_absent(&target)?;
        cancel.check()?;
        rename_for_publication(&payload, &target)?;
        *published = true;
        Ok(())
    })
}

pub(crate) fn verify_checksum_manifest(
    manifest: &Location,
    progress: &mut Progress,
) -> Result<String, String> {
    progress.results.plan([(manifest.clone(), None)]);
    progress.results.begin(manifest);
    let mut totals = Totals::default();
    match verify_manifest(manifest, progress, &mut totals) {
        Err(TransferError::Cancelled) => {
            progress.results.cancelled = true;
            Ok(format!(
                "{}\n{}",
                text("Проверка манифеста остановлена"),
                totals.message()
            ))
        }
        Err(error) => Err(error.to_string()),
        Ok(()) if totals.changed + totals.missing + totals.errors > 0 => Err(totals.message()),
        Ok(()) => Ok(totals.message()),
    }
}

fn verify_manifest(
    path: &Location,
    progress: &mut Progress,
    totals: &mut Totals,
) -> Result<(), TransferError> {
    let directory = path
        .parent()
        .ok_or_else(|| text("Нужен файл манифеста SHA-256"))?;
    let name = path
        .path()
        .file_name()
        .ok_or_else(|| text("Нужен файл манифеста SHA-256"))?;
    let cancel = progress.cancellation();
    cancel.check()?;
    if path.is_archive() {
        return Err(crate::archive::readonly().into());
    }
    let scope = Directory::open(&directory)?;
    let entries = scope
        .read_checked(Path::new(name), &cancel, |source, size, input| {
            if size > manifest::MAX_BYTES as u64 {
                return Err(manifest::too_large());
            }
            let mut bytes = Vec::with_capacity(size as usize);
            let count = progress.read_stream(source.path(), size, input, |input| {
                input.read_to_end(&mut bytes)?;
                Ok(())
            })?;
            if count != size {
                return Err(changed_during_read());
            }
            manifest::parse(&bytes, &cancel)
        })?
        .ok_or_else(|| text("Файл манифеста отсутствует"))?;

    progress.results.plan(
        entries
            .iter()
            .map(|entry| (directory.join(&entry.path), None)),
    );
    for entry in entries {
        cancel.check()?;
        let source = directory.join(&entry.path);
        progress.results.begin(&source);
        progress.item(source.path());
        let (outcome, reason) = match scope.digest(&entry.path, progress, &cancel) {
            Ok(Some(actual)) if actual == entry.digest => {
                totals.matched += 1;
                (Outcome::Completed, text("SHA-256 совпала").into())
            }
            Ok(Some(_)) => {
                totals.changed += 1;
                (
                    Outcome::Failed,
                    text("Файл изменён (SHA-256 не совпала)").into(),
                )
            }
            Ok(None) => {
                totals.missing += 1;
                (Outcome::Failed, text("Файл отсутствует").into())
            }
            Err(TransferError::Cancelled) => return Err(TransferError::Cancelled),
            Err(error) => {
                totals.errors += 1;
                (Outcome::Failed, error.to_string())
            }
        };
        progress
            .results
            .record(&source, None, outcome, Some(reason));
    }
    Ok(())
}

#[derive(Default)]
struct Totals {
    matched: usize,
    changed: usize,
    missing: usize,
    errors: usize,
}

impl Totals {
    fn message(&self) -> String {
        crate::tr_format!(
            "Совпало: {} · изменено: {} · отсутствует: {} · ошибки чтения: {}",
            "Matched: {} · changed: {} · missing: {} · read errors: {}",
            self.matched,
            self.changed,
            self.missing,
            self.errors
        )
    }
}

struct Directory {
    parent: Parent,
    #[cfg(unix)]
    descriptor: Option<std::os::fd::OwnedFd>,
}

impl Directory {
    fn open(location: &Location) -> Result<Self, TransferError> {
        if location.is_archive() {
            return Err(crate::archive::readonly().into());
        }
        let parent = Parent::capture(location)?;
        #[cfg(unix)]
        let descriptor = parent
            .location
            .local()
            .map(|path| {
                use rustix::fs::{open, Mode};
                open(path, directory_flags(), Mode::empty()).map_err(io::Error::from)
            })
            .transpose()?;
        Ok(Self {
            parent,
            #[cfg(unix)]
            descriptor,
        })
    }

    fn digest(
        &self,
        path: &Path,
        progress: &mut Progress,
        cancel: &Cancellation,
    ) -> Result<Option<[u8; 32]>, TransferError> {
        self.read_checked(path, cancel, |source, size, input| {
            progress
                .hash_file(source.path(), size, input)?
                .ok_or_else(changed_during_read)
        })
    }

    fn read_checked<T>(
        &self,
        path: &Path,
        cancel: &Cancellation,
        read: impl FnOnce(&Location, u64, &mut SourceReader) -> Result<T, TransferError>,
    ) -> Result<Option<T>, TransferError> {
        let Some(mut input) = self.input(path, cancel)? else {
            return Ok(None);
        };
        let source = self.parent.location.join(path);
        let metadata = input.metadata()?;
        if !metadata.is_file() {
            return Err(unsupported());
        }
        if matches!(&metadata, EntryMetadata::Remote(stat) if stat.size.is_none() || stat.mtime.is_none())
        {
            return Err(text("Сервер не передал размер или время изменения файла").into());
        }
        let entry = TransferSourceEntry {
            metadata,
            children: None,
        };
        let output = read(&source, entry.size(), &mut input)?;
        cancel.check()?;
        entry
            .verify_metadata(&source, &input.metadata()?)
            .map_err(|_| changed_during_read())?;
        let Some(mut current) = self.input(path, cancel)? else {
            return Err(changed_during_read());
        };
        entry
            .verify_metadata(&source, &current.metadata()?)
            .map_err(|_| changed_during_read())?;
        Ok(Some(output))
    }

    fn input(
        &self,
        path: &Path,
        cancel: &Cancellation,
    ) -> Result<Option<SourceReader>, TransferError> {
        cancel.check()?;
        #[cfg(unix)]
        if let Some(root) = &self.descriptor {
            return match local_input(root, path, cancel) {
                Ok(file) => Ok(Some(SourceReader::Local(file))),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            };
        }

        let mut directory = self.parent.location.clone();
        for component in path.parent().into_iter().flat_map(Path::components) {
            cancel.check()?;
            directory = directory.join(component.as_os_str());
            let Some(metadata) = entry_metadata(&directory)? else {
                return Ok(None);
            };
            let is_directory = match metadata {
                EntryMetadata::Local(meta) => meta.is_dir(),
                EntryMetadata::Remote(stat) => stat.is_dir(),
            };
            if !is_directory {
                return Err(unsupported());
            }
        }
        let source = directory.join(path.file_name().ok_or_else(unsupported)?);
        let Some(metadata) = entry_metadata(&source)? else {
            return Ok(None);
        };
        if !metadata.is_file() {
            return Err(unsupported());
        }
        cancel.check()?;
        let mut input = SourceReader::open(&source)?;
        let entry = TransferSourceEntry {
            metadata,
            children: None,
        };
        entry
            .verify_metadata(&source, &input.metadata()?)
            .map_err(|_| changed_during_read())?;
        Ok(Some(input))
    }
}

#[cfg(unix)]
fn directory_flags() -> rustix::fs::OFlags {
    use rustix::fs::OFlags;
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

#[cfg(unix)]
fn local_input(
    root: &std::os::fd::OwnedFd,
    path: &Path,
    cancel: &Cancellation,
) -> io::Result<std::fs::File> {
    use rustix::fs::{openat, statat, AtFlags, FileType, Mode, OFlags};

    let mut directory = None;
    for component in path.parent().into_iter().flat_map(Path::components) {
        cancel.check()?;
        directory = Some(
            openat(
                directory.as_ref().unwrap_or(root),
                component.as_os_str(),
                directory_flags(),
                Mode::empty(),
            )
            .map_err(io::Error::from)?,
        );
    }
    let parent = directory.as_ref().unwrap_or(root);
    let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            text("Для проверки подходят только обычные файлы без ссылок"),
        ));
    }
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    openat(parent, name, flags, Mode::empty())
        .map(std::fs::File::from)
        .map_err(io::Error::from)
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

fn changed_during_read() -> TransferError {
    text("Файл изменился во время чтения").into()
}

fn unsupported() -> TransferError {
    text("Для проверки подходят только обычные файлы без ссылок").into()
}

fn file_error(source: &Location, error: TransferError) -> TransferError {
    match error {
        TransferError::Failed(error) => crate::tr_format!(
            "Не удалось прочитать {source}: {error}",
            "Could not read {source}: {error}"
        )
        .into(),
        _ => error,
    }
}
