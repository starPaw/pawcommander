//! A durable working copy with checked publication back to the opened SFTP path.
use super::{
    read_scope::{changed_during_read, ReadScope},
    sftp::remote,
    staged_file::StagedFile,
    transfer::{
        entry_metadata, publish_checked_replacement, rename_for_publication,
        validate_source_child_name, with_staging_typed, EntryMetadata,
    },
    Location,
};
use crate::file_error::RecoveryNotice;
use crate::i18n::text;
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
        let parent = file.parent().ok_or_else(conflict)?;
        let metadata = regular(file)?;
        let scope = ReadScope::open(&parent)?;
        if scope.parent.location != parent {
            return Err(conflict());
        }
        let cancel = progress.cancellation();
        let digest = scope
            .digest(basename(file)?, progress, &cancel)?
            .ok_or_else(conflict)?;
        if !metadata.matches_file(&regular(file)?) {
            return Err(conflict());
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

impl RemoteEdit {
    pub(crate) fn download(
        source: Location,
        copies: &Path,
        progress: &mut Progress,
    ) -> Result<Self, TransferError> {
        if !source.remote() {
            return Err(conflict());
        }
        let scope = ReadScope::open(&source.parent().ok_or_else(conflict)?)?;
        let name = basename(&source)?;
        let target = scope.parent.location.join(name);
        let metadata = regular(&target)?;
        fs::create_dir_all(copies)?;
        let directory = tempfile::Builder::new()
            .prefix("edit-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(copies)?;
        let copy = Location::Local(directory.path().join(name));
        let digest = copy_file(&scope, name, &copy, progress)?;
        if !metadata.matches_file(&regular(&target)?) {
            return Err(conflict());
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

    pub(crate) fn save(&mut self, progress: &mut Progress) -> Result<String, String> {
        let source = Location::Local(self.copy.clone());
        progress
            .results
            .plan([(source.clone(), Some(self.target.display()))]);
        let result = self.upload(progress);
        let outcome = match &result {
            Ok(_) => Outcome::Completed,
            Err(TransferError::Cancelled) => {
                progress.results.cancelled = true;
                Outcome::Cancelled
            }
            Err(TransferError::PublicationUnconfirmed(_)) => Outcome::PublicationUnconfirmed,
            Err(_) => Outcome::Failed,
        };
        progress.results.record(
            &source,
            None,
            outcome,
            result.as_ref().err().map(crate::i18n::file_error),
        );
        result.map_err(|error| crate::i18n::file_error(&error))
    }

    fn upload(&mut self, progress: &mut Progress) -> Result<String, TransferError> {
        let cancel = progress.cancellation();
        let scope = ReadScope::open(&Location::Local(self.copy.parent().unwrap().to_owned()))?;
        let name = self.copy.file_name().map(Path::new).ok_or_else(conflict)?;
        let digest = scope
            .digest(name, progress, &cancel)?
            .ok_or_else(|| text!("Рабочая копия отсутствует."))?;
        let current = Revision::read(&self.target, progress)?;
        if current.digest == digest {
            self.revision = current;
            return Ok(text!("На сервере уже сохранена эта версия.").into());
        }
        if !self.revision.matches(&current) {
            return Err(conflict());
        }
        let mut new_revision = None;
        with_staging_typed(&self.target, Some(&cancel), |staging, preserve| {
            let payload = staging.join("payload");
            if copy_file(&scope, name, &payload, progress)? != digest
                || Revision::read(&payload, progress)?.digest != digest
            {
                return Err(changed_during_read());
            }
            let metadata = preserve_access(&payload, &current.metadata)?;
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
                        Err(conflict())
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
            new_revision = Some(Revision { metadata, digest });
            Ok(())
        })?;
        self.revision = new_revision.expect("successful publication captures its revision");
        Ok(text!("Правка сохранена на сервере.").into())
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
                return Err(changed_during_read());
            }
            writer.close()?;
            Ok(reader.finish().expect("editor copies are always hashed"))
        })?
        .ok_or_else(changed_during_read)
}

fn regular(file: &Location) -> Result<EntryMetadata, TransferError> {
    entry_metadata(file)?
        .filter(EntryMetadata::is_file)
        .ok_or_else(conflict)
}

fn basename(file: &Location) -> Result<&Path, TransferError> {
    let name = file.path().file_name().ok_or_else(conflict)?;
    validate_source_child_name(file, name)?;
    Ok(Path::new(name))
}

fn conflict() -> TransferError {
    text!("Файл на сервере изменился или недоступен. Рабочая копия сохранена.").into()
}

fn preserve_access(
    payload: &Location,
    expected: &EntryMetadata,
) -> Result<EntryMetadata, TransferError> {
    let EntryMetadata::Remote(expected) = expected else {
        return Err(conflict());
    };
    let EntryMetadata::Remote(staged) = regular(payload)? else {
        return Err(conflict());
    };
    let access = |stat: &ssh2::FileStat| (stat.uid, stat.gid, stat.perm);
    if access(&staged) == access(expected) {
        return Ok(EntryMetadata::Remote(staged));
    }
    let refused = || text!("Не удалось сохранить владельца или права доступа файла.");
    let mut changes = ssh2::FileStat {
        size: None,
        uid: None,
        gid: None,
        perm: None,
        atime: None,
        mtime: None,
    };
    if (staged.uid, staged.gid) != (expected.uid, expected.gid) {
        changes.uid = Some(expected.uid.ok_or_else(refused)?);
        changes.gid = Some(expected.gid.ok_or_else(refused)?);
    }
    let mode = expected.perm.ok_or_else(refused)?;
    // Do not publish an edited payload with set-ID or other special mode bits.
    if mode & 0o7000 != 0 {
        return Err(refused().into());
    }
    changes.perm = Some(mode & 0o777);
    remote(payload)?
        .setstat(payload.path(), changes)
        .map_err(|error| format!("{} {error}", refused()))?;
    let actual = regular(payload)?;
    if !matches!(&actual, EntryMetadata::Remote(stat) if access(stat) == access(expected)) {
        return Err(refused().into());
    }
    Ok(actual)
}
