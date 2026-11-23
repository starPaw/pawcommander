use crate::vfs::Location;

pub(crate) fn available(folder: &Location) -> bool {
    cfg!(target_os = "macos") && !folder.is_archive()
}

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub(crate) mod embedded;

#[cfg(target_os = "macos")]
mod pty;

#[cfg(target_os = "macos")]
pub(crate) use macos::{open, OpenError, OpenRequest, SshCommandError};

#[cfg(target_os = "macos")]
pub(crate) use pty::PtySpawnError;
