//! A durable working copy with checked publication back to the opened SFTP path.
use super::{
    read_scope::ReadScope,
    sftp::remote,
    staged_file::StagedFile,
    transfer::{
        entry_metadata, publish_checked_replacement, rename_for_publication,
        validate_source_child_name, with_staging_typed, EntryMetadata,
    },
    Location,
};
use crate::file_error::{
    AccessAction, AccessError, LocalPathAction, MutationError, RecoveryNotice, RemoteEditError,
    ScanError, TransferPlanError,
};
use crate::operations::{CopyReader, Progress, TransferError};
use crate::result_history::Outcome;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone)]
struct Revision {
    metadata: EntryMetadata,
    digest: [u8; 32],
}

impl Revision {
    fn read(file: &Location, progress: &mut Progress) -> Result<Self, TransferError> {
        let parent = file
            .parent()
            .ok_or_else(|| TransferPlanError::MissingParent(file.clone()))?;
        let metadata = regular(file)?;
        let scope = ReadScope::open(&parent)?;
        if scope.parent.location != parent {
            return Err(RemoteEditError::RevisionChanged(file.clone()).into());
        }
        let cancel = progress.cancellation();
        let digest = scope
            .digest(basename(file)?, progress, &cancel)?
            .ok_or_else(|| RemoteEditError::RevisionChanged(file.clone()))?;
        if !metadata.matches_file(&regular(file)?) {
            return Err(RemoteEditError::RevisionChanged(file.clone()).into());
        }
        Ok(Self { metadata, digest })
    }

    fn matches(&self, other: &Self) -> bool {
        self.digest == other.digest && self.metadata.matches_file(&other.metadata)
    }
}

#[derive(Clone)]
pub(crate) struct RemoteEdit {
    target: Location,
    copy: PathBuf,
    revision: Revision,
}

#[derive(Debug)]
pub(crate) enum SaveOutcome {
    AlreadyCurrent,
    Saved {
        cleanup_warning: Option<TransferError>,
    },
}

impl RemoteEdit {
    pub(crate) fn download(
        source: Location,
        copies: &Path,
        progress: &mut Progress,
    ) -> Result<Self, TransferError> {
        if !source.remote() {
            return Err(RemoteEditError::SftpRequired(source).into());
        }
        progress.check_cancelled()?;
        let parent = source
            .parent()
            .ok_or_else(|| TransferPlanError::MissingParent(source.clone()))?;
        let scope = ReadScope::open(&parent)?;
        let name = basename(&source)?;
        let target = scope.parent.location.join(name);
        let metadata = regular(&target)?;
        let create_error =
            |error| MutationError::local(copies, LocalPathAction::CreateDirectory, error);
        fs::create_dir_all(copies).map_err(create_error)?;
        let directory = tempfile::Builder::new()
            .prefix("edit-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(copies)
            .map_err(create_error)?;
        let copy = Location::Local(directory.path().join(name));
        let digest = copy_file(&scope, name, &copy, progress)?;
        if !metadata.matches_file(&regular(&target)?) {
            return Err(RemoteEditError::RevisionChanged(target).into());
        }
        Ok(Self {
            target,
            copy: directory.keep().join(name),
            revision: Revision { metadata, digest },
        })
    }

    pub(crate) fn target(&self) -> &Location {
        &self.target
    }

    pub(crate) fn copy(&self) -> &Path {
        &self.copy
    }

    pub(crate) fn save(&mut self, progress: &mut Progress) -> Result<SaveOutcome, TransferError> {
        let source = Location::Local(self.copy.clone());
        progress
            .results
            .plan([(source.clone(), Some(self.target.display()))]);
        progress.results.begin(&source);
        let result = self.upload(&source, progress);
        let (outcome, reason) = match &result {
            Ok(SaveOutcome::Saved {
                cleanup_warning: Some(warning),
            }) => (Outcome::CompletedWithWarning, Some(warning)),
            Ok(_) => (Outcome::Completed, None),
            Err(error @ TransferError::Cancelled) => {
                progress.results.cancelled = true;
                (Outcome::Cancelled, Some(error))
            }
            Err(error @ TransferError::PublicationUnconfirmed(_)) => {
                (Outcome::PublicationUnconfirmed, Some(error))
            }
            Err(error) => (Outcome::Failed, Some(error)),
        };
        progress.results.record_with_error(&source, outcome, reason);
        result
    }

    fn upload(
        &mut self,
        source: &Location,
        progress: &mut Progress,
    ) -> Result<SaveOutcome, TransferError> {
        progress.check_cancelled()?;
        let cancel = progress.cancellation();
        let parent = source
            .parent()
            .ok_or_else(|| TransferPlanError::MissingParent(source.clone()))?;
        let scope = ReadScope::open(&parent)?;
        let name = basename(source)?;
        let digest = scope
            .digest(name, progress, &cancel)?
            .ok_or_else(|| RemoteEditError::WorkingCopyMissing(self.copy.clone()))?;
        let current = Revision::read(&self.target, progress)?;
        if current.digest == digest {
            self.revision = current;
            return Ok(SaveOutcome::AlreadyCurrent);
        }
        if !self.revision.matches(&current) {
            return Err(RemoteEditError::RevisionChanged(self.target.clone()).into());
        }
        let mut published = false;
        let result = with_staging_typed(&self.target, Some(&cancel), |staging, preserve| {
            let payload = staging.join("payload");
            if copy_file(&scope, name, &payload, progress)? != digest
                || Revision::read(&payload, progress)?.digest != digest
            {
                return Err(ScanError::FileChanged(source.clone()).into());
            }
            let metadata = preserve_access(&payload, &self.target, &current.metadata)?;
            cancel.check()?;
            publish_checked_replacement(
                &payload,
                &self.target,
                staging,
                preserve,
                rename_for_publication,
                |candidate| {
                    let actual = Revision::read(candidate, progress)?;
                    if current.matches(&actual) {
                        Ok(())
                    } else {
                        Err(RemoteEditError::RevisionChanged(self.target.clone()).into())
                    }
                },
            )
            .map_err(|error| {
                if *preserve {
                    error.with_retained_staging(staging, RecoveryNotice::ExternalEditor)
                } else {
                    error
                }
            })?;
            self.revision = Revision { metadata, digest };
            published = true;
            Ok(())
        });
        let cleanup_warning = match result {
            Ok(()) => None,
            Err(error) if published => Some(error),
            Err(error) => return Err(error),
        };
        Ok(SaveOutcome::Saved { cleanup_warning })
    }
}

fn copy_file(
    scope: &ReadScope,
    name: &Path,
    target: &Location,
    progress: &mut Progress,
) -> Result<[u8; 32], TransferError> {
    let cancel = progress.cancellation();
    scope
        .read_checked(name, &cancel, |source, size, input| {
            let mut writer = StagedFile::open(target, cancel.clone())?;
            let mut reader = CopyReader::new(input.take(size), true);
            if progress.copy(source.path(), Some(size), &mut reader, &mut writer)? != size {
                return Err(ScanError::FileChanged(source.clone()).into());
            }
            writer.close()?;
            Ok(reader.finish().expect("editor copies are always hashed"))
        })?
        .ok_or_else(|| ScanError::FileChanged(scope.parent.location.join(name)).into())
}

fn regular(file: &Location) -> Result<EntryMetadata, TransferError> {
    entry_metadata(file)?
        .filter(EntryMetadata::is_file)
        .ok_or_else(|| RemoteEditError::RevisionChanged(file.clone()).into())
}

fn basename(file: &Location) -> Result<&Path, TransferError> {
    let name = file
        .path()
        .file_name()
        .ok_or_else(|| TransferPlanError::MissingName(file.clone()))?;
    validate_source_child_name(file, name)?;
    Ok(Path::new(name))
}

fn preserve_access(
    payload: &Location,
    target: &Location,
    expected: &EntryMetadata,
) -> Result<EntryMetadata, TransferError> {
    let EntryMetadata::Remote(expected) = expected else {
        return Err(RemoteEditError::SftpRequired(target.clone()).into());
    };
    let EntryMetadata::Remote(staged) = regular(payload)? else {
        return Err(RemoteEditError::SftpRequired(payload.clone()).into());
    };
    let access = |stat: &ssh2::FileStat| (stat.uid, stat.gid, stat.perm);
    if access(&staged) == access(expected) {
        return Ok(EntryMetadata::Remote(staged));
    }
    let missing = || RemoteEditError::MissingAccessMetadata(target.clone());
    let mut changes = ssh2::FileStat {
        size: None,
        uid: None,
        gid: None,
        perm: None,
        atime: None,
        mtime: None,
    };
    if (staged.uid, staged.gid) != (expected.uid, expected.gid) {
        changes.uid = Some(expected.uid.ok_or_else(missing)?);
        changes.gid = Some(expected.gid.ok_or_else(missing)?);
    }
    let mode = expected.perm.ok_or_else(missing)?;
    // Do not publish an edited payload with set-ID or other special mode bits.
    if mode & 0o7000 != 0 {
        return Err(RemoteEditError::UnsupportedMode(target.clone()).into());
    }
    changes.perm = Some(mode & 0o777);
    let action = AccessAction::WriteMetadata;
    remote(payload)
        .map_err(|error| AccessError::new(payload, action, error))?
        .setstat(payload.path(), changes)
        .map_err(|error| AccessError::new(payload, action, error))?;
    let actual = regular(payload)?;
    if !matches!(&actual, EntryMetadata::Remote(stat) if access(stat) == access(expected)) {
        return Err(RemoteEditError::AccessMismatch(payload.clone()).into());
    }
    Ok(actual)
}
