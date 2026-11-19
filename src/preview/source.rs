use crate::vfs::{self, Location};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    time::{Duration, SystemTime},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Version {
    pub size: Option<u64>,
    changed: Option<SystemTime>,
    identity: Option<(u64, u64)>,
}

pub(super) enum Source {
    Local(File),
    Sftp(ssh2::File),
}

impl Source {
    pub(super) fn open(location: &Location) -> Result<Self, String> {
        if let Some(path) = location.local() {
            return super::open_regular(path).map(Self::Local);
        }
        vfs::open_preview_stream(location).map(Self::Sftp)
    }

    pub(super) fn version(&mut self) -> io::Result<Version> {
        match self {
            Self::Local(file) => {
                let metadata = file.metadata()?;
                #[cfg(unix)]
                let identity = {
                    use std::os::unix::fs::MetadataExt;
                    Some((metadata.dev(), metadata.ino()))
                };
                #[cfg(not(unix))]
                let identity = None;
                Ok(Version {
                    size: Some(metadata.len()),
                    changed: metadata.modified().ok(),
                    identity,
                })
            }
            Self::Sftp(file) => {
                let stat = file.stat().map_err(io::Error::other)?;
                Ok(Version {
                    size: stat.size,
                    changed: stat.mtime.and_then(|time| {
                        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(time))
                    }),
                    identity: None,
                })
            }
        }
    }

    pub(super) fn seek_to(&mut self, byte: u64) -> io::Result<u64> {
        match self {
            Self::Local(file) => file.seek(SeekFrom::Start(byte)),
            Self::Sftp(file) => file.seek(SeekFrom::Start(byte)),
        }
    }
}

impl Read for Source {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Local(file) => file.read(bytes),
            Self::Sftp(file) => file.read(bytes),
        }
    }
}
