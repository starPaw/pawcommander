use std::{
    io::{self, Write},
    path::Path,
};

pub(super) fn write_new(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    Ok(())
}
