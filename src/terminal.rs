use crate::vfs::Location;

pub(crate) fn available(folder: &Location) -> bool {
    cfg!(target_os = "macos") && !folder.is_archive()
}

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub(crate) use macos::open;
