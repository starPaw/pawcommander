use super::{
    transfer::{check_copy_space, finish_smb_upload, mounted_smb, write_smb_payload},
    undo::Parent,
    Location,
};
use crate::{
    file_error::{MutationError, SmbMutationAction},
    operations::{Progress, TransferError},
    smb::{self, MutationMount},
};
use std::io::Read;

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

/// Publish a finished regular payload exclusively through the selected folder.
pub(super) fn publish(
    input: &mut impl Read,
    size: u64,
    source: &Location,
    target: &Location,
    parent: &Parent,
    mount: &MutationMount,
    progress: &mut Progress,
) -> Result<(), TransferError> {
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
        write_smb_payload(source, target, size, input, file, progress)
    })?;
    mount
        .recheck()
        .map_err(|cause| smb::unconfirmed_upload(target, cause))
}
