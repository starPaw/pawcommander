use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;

#[cfg(unix)]
use rustix::fs::{self, AtFlags, Dir, FileType, Mode, OFlags};
#[cfg(not(unix))]
use std::fs;

pub(super) struct Directory {
    pub path: PathBuf,
    #[cfg(unix)]
    entries: Dir,
    #[cfg(not(unix))]
    entries: fs::ReadDir,
}

pub(super) struct Entry {
    pub name: OsString,
    pub descendable: io::Result<bool>,
}

#[cfg(unix)]
impl Directory {
    const FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::CLOEXEC);

    pub fn open(path: PathBuf) -> io::Result<Self> {
        let entries = Dir::new(fs::open(&path, Self::FLAGS, Mode::empty())?)?;
        Ok(Self { path, entries })
    }

    pub fn next_entry(&mut self) -> io::Result<Option<Entry>> {
        use std::os::unix::ffi::OsStrExt;

        for entry in self.entries.by_ref() {
            let entry = entry?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name == "." || name == ".." {
                continue;
            }
            let descendable = match entry.file_type() {
                FileType::Directory => Ok(true),
                FileType::Unknown => self
                    .entries
                    .fd()
                    .and_then(|fd| fs::statat(fd, name, AtFlags::SYMLINK_NOFOLLOW))
                    .map(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::Directory)
                    .map_err(io::Error::from),
                _ => Ok(false),
            };
            return Ok(Some(Entry {
                name: name.to_owned(),
                descendable,
            }));
        }
        Ok(None)
    }

    pub fn open_child(&self, name: &OsStr) -> io::Result<Option<Self>> {
        // Resolve only this entry relative to the open parent, so concurrent
        // replacement of an ancestor or child with a symlink cannot redirect us.
        let fd = fs::openat(
            self.entries.fd()?,
            name,
            Self::FLAGS | OFlags::NOFOLLOW,
            Mode::empty(),
        )?;
        Ok(Some(Self {
            path: self.path.join(name),
            entries: Dir::new(fd)?,
        }))
    }
}

#[cfg(not(unix))]
impl Directory {
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let entries = fs::read_dir(&path)?;
        Ok(Self { path, entries })
    }

    pub fn next_entry(&mut self) -> io::Result<Option<Entry>> {
        self.entries.next().transpose().map(|entry| {
            entry.map(|entry| Entry {
                name: entry.file_name(),
                descendable: entry.metadata().map(|metadata| descendable(&metadata)),
            })
        })
    }

    pub fn open_child(&self, name: &OsStr) -> io::Result<Option<Self>> {
        let path = self.path.join(name);
        // Revalidate before opening a path. A concurrent replacement after
        // this check can still change what the operating system opens.
        if !descendable(&fs::symlink_metadata(&path)?) {
            return Ok(None);
        }
        Self::open(path).map(Some)
    }
}

#[cfg(not(unix))]
fn descendable(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    metadata.is_dir()
}
