mod manifest;
#[cfg(test)]
mod tests;
#[cfg(all(test, target_os = "macos"))]
pub(super) use tests::checksum_round_trip;

use super::{
    location::validate_name,
    read_scope::{changed_during_read, ReadScope},
    staged_file::StagedFile,
    transfer::{canonical, entry_metadata, rename_for_publication, with_staging_typed},
    Location,
};
use crate::{
    i18n::text,
    operations::{Progress, TransferError},
    result_history::Outcome,
};
use std::{io::Write, path::Path};

pub(crate) fn checksum_manifest_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    validate_name(name).map_err(|_| text!("Введите допустимое имя манифеста").to_owned())?;
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
    progress.results.plan_selected_items([target.clone()]);
    progress.results.begin(&target);
    let mut published = false;
    match write_manifest(directory, name, sources, progress, &mut published) {
        Ok(()) => Ok(crate::tr_format!(
            "SHA-256 manifest created: {target}",
            target = target
        )),
        Err(TransferError::Cancelled) => {
            progress.results.cancelled = true;
            Ok(text!("Создание манифеста остановлено").into())
        }
        Err(error) => {
            let outcome = if published {
                Outcome::CompletedWithWarning
            } else if matches!(error, TransferError::PublicationUnconfirmed(_)) {
                Outcome::PublicationUnconfirmed
            } else {
                Outcome::Failed
            };
            let message = crate::i18n::file_error(&error);
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
        return Err(text!("Выберите обычные файлы из текущей папки").into());
    }
    if sources.len() > manifest::MAX_ENTRIES {
        return Err(manifest::too_large());
    }
    let cancel = progress.cancellation();
    cancel.check()?;
    let scope = ReadScope::open(directory)?;
    let target = scope.parent.location.join(name);
    ensure_absent(&target)?;
    let mut body = Vec::new();
    for source in sources {
        cancel.check()?;
        let relative = Path::new(
            source
                .path()
                .file_name()
                .ok_or_else(|| text!("Выберите обычные файлы из текущей папки"))?,
        );
        let digest = scope
            .digest(relative, progress, &cancel)
            .map_err(|error| file_error(source, error))?
            .ok_or_else(|| crate::tr_format!("File is missing: {source}", source = source))?;
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
            return Err(text!("Папка манифеста изменилась").into());
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
    progress.results.plan_selected_items([manifest.clone()]);
    progress.results.begin(manifest);
    let mut totals = Totals::default();
    match verify_manifest(manifest, progress, &mut totals) {
        Err(TransferError::Cancelled) => {
            progress.results.cancelled = true;
            Ok(format!(
                "{}\n{}",
                text!("Проверка манифеста остановлена"),
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
        .ok_or_else(|| text!("Нужен файл манифеста SHA-256"))?;
    let name = path
        .path()
        .file_name()
        .ok_or_else(|| text!("Нужен файл манифеста SHA-256"))?;
    let cancel = progress.cancellation();
    cancel.check()?;
    if path.is_archive() {
        return Err(crate::archive::readonly().into());
    }
    let scope = ReadScope::open(&directory)?;
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
        .ok_or_else(|| text!("Файл манифеста отсутствует"))?;

    progress
        .results
        .plan_selected_items(entries.iter().map(|entry| directory.join(&entry.path)));
    for entry in entries {
        cancel.check()?;
        let source = directory.join(&entry.path);
        progress.results.begin(&source);
        progress.item(source.path());
        let (outcome, reason) = match scope.digest(&entry.path, progress, &cancel) {
            Ok(Some(actual)) if actual == entry.digest => {
                totals.matched += 1;
                (Outcome::Completed, text!("SHA-256 совпала").into())
            }
            Ok(Some(_)) => {
                totals.changed += 1;
                (
                    Outcome::Failed,
                    text!("Файл изменён (SHA-256 не совпала)").into(),
                )
            }
            Ok(None) => {
                totals.missing += 1;
                (Outcome::Failed, text!("Файл отсутствует").into())
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
            "Matched: {} · changed: {} · missing: {} · read errors: {}",
            self.matched,
            self.changed,
            self.missing,
            self.errors
        )
    }
}

fn ensure_absent(target: &Location) -> Result<(), TransferError> {
    if entry_metadata(target)?.is_some() {
        return Err(
            crate::tr_format!("File or folder already exists: {target}", target = target).into(),
        );
    }
    Ok(())
}

fn file_error(source: &Location, error: TransferError) -> TransferError {
    match error {
        error @ TransferError::Failed(_) => crate::tr_format!(
            "Could not read {source}: {error}",
            error = crate::i18n::file_error(&error),
            source = source
        )
        .into(),
        _ => error,
    }
}
