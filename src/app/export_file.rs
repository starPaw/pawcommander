use crate::{
    file_error::{ExportIoAction, ExportProblem, ExportTarget},
    operations::TransferError,
};
use std::io::{self, Write};

pub(super) fn write_new(target: &ExportTarget, contents: &[u8]) -> Result<(), TransferError> {
    let error = |action, source| target.failure(ExportProblem::Io { action, source });
    let parent = target.path.parent().ok_or_else(|| {
        error(
            ExportIoAction::CreateTemporary,
            io::Error::from(io::ErrorKind::InvalidInput),
        )
    })?;
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .map_err(|source| error(ExportIoAction::CreateTemporary, source))?;
    file.write_all(contents)
        .map_err(|source| error(ExportIoAction::Write, source))?;
    file.as_file()
        .sync_all()
        .map_err(|source| error(ExportIoAction::Sync, source))?;
    file.persist_noclobber(&target.path)
        .map_err(|failure| error(ExportIoAction::Publish, failure.error))?;
    Ok(())
}
