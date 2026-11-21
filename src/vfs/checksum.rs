mod manifest;
#[cfg(test)]
mod tests;
#[cfg(all(test, target_os = "macos"))]
pub(super) use tests::checksum_round_trip;

use super::{
    location::validate_name,
    read_scope::ReadScope,
    staged_file::StagedFile,
    transfer::{canonical, entry_metadata, rename_for_publication, with_staging_typed},
    Location,
};
use crate::{
    file_error::{AccessError, ChecksumError, MutationError, ScanError},
    operations::{Progress, TransferError},
    result_history::{ChecksumOutcome, Outcome},
};
use std::{io::Write, path::Path};

pub(crate) fn checksum_manifest_name(name: &str) -> Result<&str, TransferError> {
    let name = name.trim();
    validate_name(name).map_err(ChecksumError::InvalidName)?;
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
) -> Result<Location, TransferError> {
    let name = checksum_manifest_name(name)?;
    let target = directory.join(name);
    progress.results.plan_selected_items([target.clone()]);
    progress.results.begin(&target);
    let mut published = false;
    match write_manifest(directory, name, sources, progress, &mut published) {
        Ok(()) => {
            progress
                .results
                .record(&target, None, Outcome::Completed, None);
            Ok(target)
        }
        Err(error @ TransferError::Cancelled) => {
            progress.results.cancelled = true;
            Err(error)
        }
        Err(error) => {
            let outcome = if published {
                Outcome::CompletedWithWarning
            } else if matches!(error, TransferError::PublicationUnconfirmed(_)) {
                Outcome::PublicationUnconfirmed
            } else {
                Outcome::Failed
            };
            progress
                .results
                .record_with_error(&target, outcome, Some(&error));
            Err(error)
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
        return Err(ChecksumError::InvalidSelection(directory.clone()).into());
    }
    if sources.len() > manifest::MAX_ENTRIES {
        return Err(ChecksumError::TooLarge(directory.join(name)).into());
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
                .ok_or_else(|| ChecksumError::InvalidSelection(directory.clone()))?,
        );
        let digest = scope
            .digest(relative, progress, &cancel)?
            .ok_or_else(|| ChecksumError::SourceMissing(source.clone()))?;
        manifest::append(&mut body, &target, source, relative, digest)?;
    }

    progress.item(target.path());
    with_staging_typed(&target, Some(&cancel), |staging, _| {
        let payload = staging.join("payload");
        let mut output = StagedFile::open(&payload, cancel.clone())?;
        for bytes in body.chunks(64 * 1024) {
            output
                .write_all(bytes)
                .map_err(|error| AccessError::write(&payload, error))?;
        }
        output.close()?;
        scope.verify_root()?;
        if canonical(directory)? != scope.parent.location.path() {
            return Err(ChecksumError::DirectoryChanged(directory.clone()).into());
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
) -> Result<ChecksumVerification, TransferError> {
    progress.results.plan_selected_items([manifest.clone()]);
    progress.results.begin(manifest);
    let mut report = ChecksumVerification::default();
    match verify_manifest(manifest, progress, &mut report) {
        Err(TransferError::Cancelled) => {
            progress.results.cancelled = true;
            report.cancelled = true;
        }
        Err(error) => {
            progress
                .results
                .record_with_error(manifest, Outcome::Failed, Some(&error));
            return Err(error);
        }
        Ok(()) => {}
    }
    Ok(report)
}

fn verify_manifest(
    path: &Location,
    progress: &mut Progress,
    report: &mut ChecksumVerification,
) -> Result<(), TransferError> {
    let directory = path
        .parent()
        .ok_or_else(|| ChecksumError::RequiresManifest(path.clone()))?;
    let name = path
        .path()
        .file_name()
        .ok_or_else(|| ChecksumError::RequiresManifest(path.clone()))?;
    let cancel = progress.cancellation();
    cancel.check()?;
    if path.is_archive() {
        return Err(MutationError::ReadOnlyArchive(path.clone()).into());
    }
    let scope = ReadScope::open(&directory)?;
    let entries = scope
        .read_checked(Path::new(name), &cancel, |source, size, input| {
            if size > manifest::MAX_BYTES as u64 {
                return Err(ChecksumError::TooLarge(source.clone()).into());
            }
            let mut bytes = Vec::with_capacity(size as usize);
            let count = progress
                .read_stream(source.path(), size, input, |input| {
                    input.read_to_end(&mut bytes)?;
                    Ok(())
                })
                .map_err(|error| AccessError::read(source, error))?;
            if count != size {
                return Err(ScanError::FileChanged(source.clone()).into());
            }
            manifest::parse(&bytes, source, &cancel)
        })?
        .ok_or_else(|| ChecksumError::ManifestMissing(path.clone()))?;

    progress
        .results
        .plan_selected_items(entries.iter().map(|entry| directory.join(&entry.path)));
    for entry in entries {
        cancel.check()?;
        let source = directory.join(&entry.path);
        progress.results.begin(&source);
        progress.item(source.path());
        let outcome = match scope.digest(&entry.path, progress, &cancel) {
            Ok(Some(actual)) if actual == entry.digest => {
                report.matched += 1;
                ChecksumOutcome::Matched
            }
            Ok(Some(_)) => {
                report.changed += 1;
                ChecksumOutcome::Changed
            }
            Ok(None) => {
                report.missing += 1;
                ChecksumOutcome::Missing
            }
            Err(TransferError::Cancelled) => return Err(TransferError::Cancelled),
            Err(error) => {
                report.errors += 1;
                ChecksumOutcome::Unreadable(error)
            }
        };
        progress.results.record_checksum(&source, outcome);
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ChecksumVerification {
    pub(crate) matched: usize,
    pub(crate) changed: usize,
    pub(crate) missing: usize,
    pub(crate) errors: usize,
    pub(crate) cancelled: bool,
}

fn ensure_absent(target: &Location) -> Result<(), TransferError> {
    if entry_metadata(target)?.is_some() {
        return Err(MutationError::DestinationExists(target.clone()).into());
    }
    Ok(())
}
