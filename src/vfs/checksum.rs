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
    operations::{Cancellation, Progress, TransferError},
    result_history::{ChecksumOutcome, Outcome},
};
use std::{io::Write, path::Path};
#[cfg(target_os = "macos")]
use {super::smb_output, crate::file_error::SmbMutationAction};

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
    #[cfg(target_os = "macos")]
    let mut mount = smb_output::guard(
        &directory.join(name),
        SmbMutationAction::CreateChecksumManifest,
    )?;
    #[cfg(target_os = "macos")]
    let scope = ReadScope::open_with_smb(directory, &mut mount)?;
    #[cfg(not(target_os = "macos"))]
    let scope = ReadScope::open(directory)?;
    let target = scope.parent.location.join(name);
    #[cfg(target_os = "macos")]
    let smb_destination = smb_output::is_destination(&mut mount, &target)?;
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
    let verify_destination = || -> Result<(), TransferError> {
        scope.verify_root()?;
        if canonical(directory)? != scope.parent.location.path() {
            return Err(ChecksumError::DirectoryChanged(directory.clone()).into());
        }
        ensure_absent(&target)?;
        cancel.check()?;
        Ok(())
    };
    #[cfg(target_os = "macos")]
    if smb_destination {
        verify_destination()?;
        smb_output::publish(
            &mut body.as_slice(),
            body.len() as u64,
            &target,
            &target,
            &scope.parent,
            &mount,
            progress,
        )?;
        *published = true;
        return Ok(());
    }
    with_staging_typed(&target, Some(&cancel), |staging, _| {
        let payload = staging.join("payload");
        let mut output = StagedFile::open(&payload, cancel.clone())?;
        for bytes in body.chunks(64 * 1024) {
            output
                .write_all(bytes)
                .map_err(|error| AccessError::write(&payload, error))?;
        }
        output.close()?;
        verify_destination()?;
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
        Err(error) => return Err(error),
        Ok(()) => {}
    }
    Ok(report)
}

fn verify_manifest(
    path: &Location,
    progress: &mut Progress,
    report: &mut ChecksumVerification,
) -> Result<(), TransferError> {
    let cancel = progress.cancellation();
    let (directory, scope, entries) =
        prepare_verification(path, progress, &cancel).inspect_err(|error| {
            if !matches!(error, TransferError::Cancelled) {
                progress
                    .results
                    .record_with_error(path, Outcome::Failed, Some(error));
            }
        })?;

    progress
        .results
        .plan_selected_items(entries.iter().map(|entry| directory.join(&entry.path)));
    for entry in entries {
        cancel.check()?;
        let source = directory.join(&entry.path);
        progress.results.begin(&source);
        progress.item(source.path());
        let hashed = match scope.digest(&entry.path, progress, &cancel) {
            Err(TransferError::Cancelled) => return Err(TransferError::Cancelled),
            hashed => hashed,
        };
        // A lost mount interrupts the batch and leaves pending rows unstarted.
        #[cfg(target_os = "macos")]
        if let Err(error) = scope.verify_mount() {
            progress
                .results
                .record_with_error(&source, Outcome::Failed, Some(&error));
            return Err(error);
        }
        let outcome = match hashed {
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
            Err(error) => {
                report.errors += 1;
                ChecksumOutcome::Unreadable(error)
            }
        };
        progress.results.record_checksum(&source, outcome);
    }
    #[cfg(target_os = "macos")]
    scope.verify_mount()?;
    Ok(())
}

fn prepare_verification(
    path: &Location,
    progress: &mut Progress,
    cancel: &Cancellation,
) -> Result<(Location, ReadScope, Vec<manifest::Entry>), TransferError> {
    let directory = path
        .parent()
        .ok_or_else(|| ChecksumError::RequiresManifest(path.clone()))?;
    let name = path
        .path()
        .file_name()
        .ok_or_else(|| ChecksumError::RequiresManifest(path.clone()))?;
    cancel.check()?;
    if path.is_archive() {
        return Err(MutationError::ReadOnlyArchive(path.clone()).into());
    }
    #[cfg(target_os = "macos")]
    let scope = {
        let mut mount = smb_output::guard(path, SmbMutationAction::VerifyChecksumManifest)?;
        ReadScope::open_with_smb(&directory, &mut mount)?
    };
    #[cfg(not(target_os = "macos"))]
    let scope = ReadScope::open(&directory)?;
    let entries = scope.read_checked(Path::new(name), cancel, |source, size, input| {
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
        manifest::parse(&bytes, source, cancel)
    });
    #[cfg(target_os = "macos")]
    if !matches!(&entries, Err(TransferError::Cancelled)) {
        scope.verify_mount()?;
    }
    let entries = entries?.ok_or_else(|| ChecksumError::ManifestMissing(path.clone()))?;
    Ok((directory, scope, entries))
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
