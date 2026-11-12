use chrono::{DateTime, TimeDelta, Utc};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
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
    pub kind: io::Result<EntryKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EntryKind {
    Directory,
    File,
    Other,
}

pub(super) struct EntryMetadata {
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
}

impl EntryMetadata {
    pub fn for_open_file(metadata: &Metadata) -> Self {
        Self {
            kind: EntryKind::File,
            size: metadata.len(),
            modified: modified_time(metadata),
        }
    }
}

fn modified_time(metadata: &Metadata) -> Option<DateTime<Utc>> {
    use std::time::SystemTime;
    match metadata
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
    {
        Ok(duration) => {
            DateTime::UNIX_EPOCH.checked_add_signed(TimeDelta::from_std(duration).ok()?)
        }
        Err(error) => {
            DateTime::UNIX_EPOCH.checked_sub_signed(TimeDelta::from_std(error.duration()).ok()?)
        }
    }
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
            let kind = match entry.file_type() {
                FileType::Unknown => self
                    .entries
                    .fd()
                    .and_then(|fd| fs::statat(fd, name, AtFlags::SYMLINK_NOFOLLOW))
                    .map(|stat| entry_kind(FileType::from_raw_mode(stat.st_mode)))
                    .map_err(io::Error::from),
                kind => Ok(entry_kind(kind)),
            };
            return Ok(Some(Entry {
                name: name.to_owned(),
                kind,
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

    pub fn entry_metadata(&self, name: &OsStr) -> io::Result<EntryMetadata> {
        let stat = fs::statat(self.entries.fd()?, name, AtFlags::SYMLINK_NOFOLLOW)?;
        // The width of time_t varies between targets. Day precision only needs seconds.
        #[allow(clippy::unnecessary_cast)]
        let modified = DateTime::from_timestamp(stat.st_mtime as i64, 0);
        Ok(EntryMetadata {
            kind: entry_kind(FileType::from_raw_mode(stat.st_mode)),
            size: u64::try_from(stat.st_size).map_err(|_| io::ErrorKind::InvalidData)?,
            modified,
        })
    }

    pub fn open_file(&self, name: &OsStr) -> io::Result<Option<(File, Metadata)>> {
        let parent = self.entries.fd()?;
        let stat = fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Ok(None);
        }
        // Anchor the open to the same directory even if an ancestor is moved.
        // NOFOLLOW refuses links, and NONBLOCK avoids waiting on a swapped FIFO.
        let file = File::from(fs::openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let metadata = file.metadata()?;
        Ok(metadata.is_file().then_some((file, metadata)))
    }
}

#[cfg(unix)]
fn entry_kind(kind: FileType) -> EntryKind {
    match kind {
        FileType::Directory => EntryKind::Directory,
        FileType::RegularFile => EntryKind::File,
        _ => EntryKind::Other,
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
                kind: entry.metadata().map(|metadata| entry_kind(&metadata)),
            })
        })
    }

    pub fn open_child(&self, name: &OsStr) -> io::Result<Option<Self>> {
        let path = self.path.join(name);
        // Revalidate before opening a path. A concurrent replacement after
        // this check can still change what the operating system opens.
        if entry_kind(&fs::symlink_metadata(&path)?) != EntryKind::Directory {
            return Ok(None);
        }
        Self::open(path).map(Some)
    }

    pub fn entry_metadata(&self, name: &OsStr) -> io::Result<EntryMetadata> {
        let metadata = fs::symlink_metadata(self.path.join(name))?;
        Ok(EntryMetadata {
            kind: entry_kind(&metadata),
            size: metadata.len(),
            modified: modified_time(&metadata),
        })
    }

    pub fn open_file(&self, name: &OsStr) -> io::Result<Option<(File, Metadata)>> {
        let path = self.path.join(name);
        if entry_kind(&fs::symlink_metadata(&path)?) != EntryKind::File {
            return Ok(None);
        }
        let file = crate::fs_ops::open_regular_source(&path)?;
        let metadata = file.metadata()?;
        Ok((entry_kind(&metadata) == EntryKind::File).then_some((file, metadata)))
    }
}

#[cfg(not(unix))]
fn entry_kind(metadata: &Metadata) -> EntryKind {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return EntryKind::Other;
        }
    }
    if metadata.is_dir() {
        EntryKind::Directory
    } else if metadata.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}
