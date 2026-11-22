use super::{smb_output, staged_file::StagedFile, undo::Parent, Location};
use crate::{
    file_error::ArchiveError,
    operations::{Cancellation, Progress, TransferError},
    smb::MutationMount,
};
use std::{fs::File, io::Seek};

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
    smb_output::publish(&mut input, size, source, target, parent, mount, progress)
}
