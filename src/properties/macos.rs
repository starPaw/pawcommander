use super::Summary;
use std::ffi::{CStr, CString, OsStr};
use std::fs::{File, Metadata};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) fn scan_root(
    mut path: PathBuf,
    metadata: Metadata,
    cancel: &AtomicBool,
    summary: &mut Summary,
    progress: &mut impl FnMut(&Summary),
) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    summary.count(&metadata);
    if !metadata.is_dir() || cancel.load(Ordering::Relaxed) {
        progress(summary);
        return;
    }

    let root = CString::new(path.as_os_str().as_bytes())
        .map_err(io::Error::from)
        .and_then(|name| Directory::open(libc::AT_FDCWD, &name, (metadata.dev(), metadata.ino())));
    let root = match root {
        Ok(root) => root,
        Err(error) => {
            summary.error(&path, error);
            progress(summary);
            return;
        }
    };

    // Keep one open stream per level. Path strings are only for diagnostics
    // after the root is open, so replacing an ancestor cannot redirect a lookup.
    let mut directories = vec![root];
    progress(summary);
    while !cancel.load(Ordering::Relaxed) {
        let Some(directory) = directories.last_mut() else {
            break;
        };
        let entry = match directory.next() {
            Ok(Some(entry)) => entry,
            result => {
                if let Err(error) = result {
                    summary.error(&path, error);
                    progress(summary);
                }
                directories.pop();
                path.pop();
                continue;
            }
        };
        let name = OsStr::from_bytes(entry.name.to_bytes());
        match entry.metadata() {
            Ok(observed) if entry_type(&observed) == libc::S_IFDIR => {
                progress(summary);
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                path.push(name);
                match entry.descend(&observed, summary) {
                    Ok(Some(child)) => directories.push(child),
                    Ok(None) => {
                        path.pop();
                    }
                    Err(error) => {
                        summary.error(&path, error);
                        path.pop();
                    }
                }
            }
            Ok(stat) => count_entry(summary, &stat),
            Err(error) => summary.error(&path.join(name), error),
        }
        progress(summary);
    }
}

struct Directory(NonNull<libc::DIR>);

impl Directory {
    fn open(parent: RawFd, name: &CStr, expected: (u64, u64)) -> io::Result<Self> {
        // SAFETY: name is NUL terminated and parent is AT_FDCWD or a live directory fd.
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful open returned a fresh fd owned by this function.
        let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let actual = file.metadata()?;
        if (actual.dev(), actual.ino()) != expected {
            return Err(changed_directory());
        }
        // SAFETY: file is a live directory opened for reading. fdopendir takes
        // ownership on success only.
        let stream = unsafe { libc::fdopendir(file.as_raw_fd()) };
        let stream = NonNull::new(stream).ok_or_else(io::Error::last_os_error)?;
        let _ = file.into_raw_fd();
        Ok(Self(stream))
    }

    fn next(&mut self) -> io::Result<Option<DirectoryEntry<'_>>> {
        loop {
            // SAFETY: this stream is live and exclusively borrowed. Clear the
            // thread-local errno immediately before readdir to distinguish EOF.
            let entry = unsafe {
                *libc::__error() = 0;
                libc::readdir(self.0.as_ptr())
            };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                return if error.raw_os_error() == Some(0) {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            // SAFETY: readdir supplies a NUL-terminated name valid until the
            // next call on this stream. The entry borrows the stream until then.
            // Project the raw field without borrowing its full declared array.
            let name = unsafe { CStr::from_ptr((&raw const (*entry).d_name).cast()) };
            if !matches!(name.to_bytes(), b"." | b"..") {
                return Ok(Some(DirectoryEntry { parent: self, name }));
            }
        }
    }

    fn fd(&self) -> RawFd {
        // SAFETY: this stream owns a live directory descriptor.
        unsafe { libc::dirfd(self.0.as_ptr()) }
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        // SAFETY: this stream is owned by self. closedir also closes its fd.
        unsafe { libc::closedir(self.0.as_ptr()) };
    }
}

struct DirectoryEntry<'a> {
    parent: &'a Directory,
    name: &'a CStr,
}

impl DirectoryEntry<'_> {
    fn metadata(&self) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::uninit();
        // SAFETY: the directory and its NUL-terminated entry name are borrowed
        // from the live stream, and stat provides writable storage for fstatat.
        let result = unsafe {
            libc::fstatat(
                self.parent.fd(),
                self.name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstatat initialized stat on success.
        Ok(unsafe { stat.assume_init() })
    }

    fn descend(
        &self,
        observed: &libc::stat,
        summary: &mut Summary,
    ) -> io::Result<Option<Directory>> {
        let current = self.metadata()?;
        if entry_type(&current) != libc::S_IFDIR {
            count_entry(summary, &current);
            return Ok(None);
        }
        if (current.st_dev, current.st_ino) != (observed.st_dev, observed.st_ino) {
            return Err(changed_directory());
        }
        count_entry(summary, &current);
        Directory::open(
            self.parent.fd(),
            self.name,
            (current.st_dev as u64, current.st_ino),
        )
        .map(Some)
    }
}

fn entry_type(stat: &libc::stat) -> libc::mode_t {
    stat.st_mode & libc::S_IFMT
}

fn count_entry(summary: &mut Summary, stat: &libc::stat) {
    match entry_type(stat) {
        libc::S_IFLNK => summary.links += 1,
        libc::S_IFREG => {
            summary.files += 1;
            summary.bytes = summary.bytes.saturating_add(stat.st_size.max(0) as u64);
        }
        libc::S_IFDIR => summary.directories += 1,
        _ => summary.other += 1,
    }
}

fn changed_directory() -> io::Error {
    io::Error::other(crate::i18n::text("Папка изменилась во время подсчёта"))
}
