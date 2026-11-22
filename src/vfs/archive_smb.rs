use super::{
    staged_file::StagedFile,
    transfer::{check_copy_space, finish_smb_upload, mounted_smb, write_smb_payload},
    undo::Parent,
    Location,
};
use crate::{
    file_error::{ArchiveError, MutationError, SmbMutationAction},
    operations::{Cancellation, Progress, TransferError},
    smb::{self, MutationMount},
};
use std::{fs::File, io::Seek};

pub(super) fn guard(
    location: &Location,
    action: SmbMutationAction,
) -> Result<MutationMount, TransferError> {
    let mut mount = MutationMount::new(action);
    check(&mut mount, location)?;
    Ok(mount)
}

pub(super) fn check(mount: &mut MutationMount, location: &Location) -> Result<(), TransferError> {
    if let Some(path) = location.local() {
        mount.check(path)?;
    }
    Ok(())
}

pub(super) fn is_destination(
    mount: &mut MutationMount,
    target: &Location,
) -> Result<bool, TransferError> {
    check(mount, target)?;
    mount.recheck()?;
    Ok(mount.is_owned() || mounted_smb(target))
}

/// Finish generating and validating a regular payload before it can appear on SMB.
pub(super) fn spool(
    target: &Location,
    cancel: &Cancellation,
    generate: impl FnOnce(StagedFile) -> Result<(), TransferError>,
) -> Result<File, TransferError> {
    cancel.check()?;
    let stage_error = |source| ArchiveError::StageOutput {
        target: target.clone(),
        source,
    };
    let input = tempfile::tempfile().map_err(stage_error)?;
    let output = input.try_clone().map_err(stage_error)?;
    generate(StagedFile::from_local(output, target, cancel.clone()))?;
    Ok(input)
}

pub(super) fn publish(
    mut input: File,
    source: &Location,
    target: &Location,
    parent: &Parent,
    mount: &MutationMount,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let stage_error = |source| ArchiveError::StageOutput {
        target: target.clone(),
        source,
    };
    let size = input.metadata().map_err(stage_error)?.len();
    input.rewind().map_err(stage_error)?;
    mount.recheck()?;
    check_copy_space(size, target, target)?;
    progress.checkpoint()?;
    let directory = match mount.open_parent(target.path())? {
        Some(directory) => directory,
        None => smb::open_upload_parent(target, None)?,
    };
    if !parent.matches_handle(&directory)? {
        return Err(MutationError::PlanChanged(parent.location.clone()).into());
    }
    let output = smb::create_upload_at(target, &directory)?;
    finish_smb_upload(target, output, |file| {
        write_smb_payload(source, target, size, &mut input, file, progress)
    })?;
    mount
        .recheck()
        .map_err(|cause| smb::unconfirmed_upload(target, cause))
}
