use super::{
    listing::next_sftp_entry,
    location::normalize_remote,
    sftp::{remote, should_reopen},
    Location, MAX_TREE_DEPTH,
};
use crate::i18n::text;
use ssh2::{Error, File, FileStat, Sftp};
use std::{
    collections::HashSet,
    path::{Component, Path},
    sync::atomic::{AtomicBool, Ordering},
};

pub(crate) enum PropertyEvent<'a> {
    Entry {
        location: &'a Location,
        stat: &'a FileStat,
        selected: bool,
    },
    Error(&'a Location, String),
}

/// Reads selected roots and their descendants on one authenticated channel.
pub(crate) fn visit_properties(
    sources: &[Location],
    cancel: &AtomicBool,
    mut visit: impl FnMut(PropertyEvent<'_>),
) {
    let Some(first) = sources.first() else {
        return;
    };
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    let channel = remote(first);
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    let channel = match channel {
        Ok(channel) => channel,
        Err(error) => {
            visit(PropertyEvent::Error(first, error));
            return;
        }
    };
    let mut scanner = Scanner {
        channel: &channel,
        cancel,
        visit,
    };
    let mut roots: Vec<_> = sources.iter().collect();
    roots.sort_by_key(|source| source.path());
    roots.dedup();
    let mut directories = HashSet::new();
    for source in roots {
        if scanner.cancelled() {
            break;
        }
        if source
            .path()
            .ancestors()
            .skip(1)
            .any(|parent| directories.contains(parent))
        {
            continue;
        }
        let (location, stat) = match scanner.root_metadata(source) {
            Ok(root) => root,
            Err(error) => {
                if !scanner.report(source, error) {
                    break;
                }
                continue;
            }
        };
        (scanner.visit)(PropertyEvent::Entry {
            location: source,
            stat: &stat,
            selected: true,
        });
        if stat.is_dir() {
            directories.insert(source.path());
            if !scanner.scan_directory(&location, &stat, 0) {
                break;
            }
        }
    }
}

enum ReadError {
    Cancelled,
    Server(Error),
    Path(crate::operations::TransferError),
}

struct Scanner<'a, F> {
    channel: &'a Sftp,
    cancel: &'a AtomicBool,
    visit: F,
}

impl<F: FnMut(PropertyEvent<'_>)> Scanner<'_, F> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn request<T>(&self, request: impl FnOnce() -> Result<T, Error>) -> Result<T, ReadError> {
        if self.cancelled() {
            return Err(ReadError::Cancelled);
        }
        let result = request();
        if self.cancelled() {
            return Err(ReadError::Cancelled);
        }
        result.map_err(ReadError::Server)
    }

    /// Returns whether to continue with siblings and other roots.
    fn report(&mut self, location: &Location, error: ReadError) -> bool {
        let (message, keep_reading) = match error {
            ReadError::Cancelled => return false,
            ReadError::Server(error) => (error.to_string(), !should_reopen(&error)),
            ReadError::Path(error) => (crate::i18n::file_error(&error), true),
        };
        (self.visit)(PropertyEvent::Error(location, message));
        keep_reading
    }

    fn root_metadata(&self, source: &Location) -> Result<(Location, FileStat), ReadError> {
        let path = normalize_remote(source.path()).map_err(ReadError::Path)?;
        let (parent, name) = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => (parent, Some(name)),
            _ => (path.as_path(), None),
        };
        // The selected item can be in a directory reached through an alias.
        // Resolve that directory, then lstat the item itself.
        let canonical = self.request(|| self.channel.realpath(parent))?;
        let canonical = normalize_remote(&canonical).map_err(ReadError::Path)?;
        let path = name.map_or_else(|| canonical.clone(), |name| canonical.join(name));
        let stat = self.request(|| self.channel.lstat(&path))?;
        self.check_canonical(&canonical)?;
        Ok((source.with_path(path), stat))
    }

    fn check_canonical(&self, path: &Path) -> Result<(), ReadError> {
        let resolved = self.request(|| self.channel.realpath(path))?;
        if resolved.as_os_str() == path.as_os_str() {
            Ok(())
        } else {
            Err(changed_directory())
        }
    }

    fn check_directory(&self, path: &Path, expected: &FileStat) -> Result<FileStat, ReadError> {
        let actual = self.request(|| self.channel.lstat(path))?;
        // Access time can change when a directory is opened. It is not a
        // useful version check. READDIR can also omit optional attributes.
        if actual.perm != expected.perm
            || differs(actual.uid, expected.uid)
            || differs(actual.gid, expected.gid)
            || differs(actual.size, expected.size)
            || differs(actual.mtime, expected.mtime)
        {
            return Err(changed_directory());
        }
        self.check_canonical(path)?;
        Ok(actual)
    }

    fn open_directory(&self, location: &Location, stat: &FileStat) -> Result<File, ReadError> {
        let path = location.path();
        let before = self.check_directory(path, stat)?;
        let directory = self.request(|| self.channel.opendir(path))?;
        // SFTP resolves paths on the server, so recheck before reading the
        // handle in case this path changed during OPENDIR.
        self.check_directory(path, &before)?;
        Ok(directory)
    }

    fn scan_directory(&mut self, location: &Location, stat: &FileStat, depth: usize) -> bool {
        if self.cancelled() {
            return false;
        }
        if depth >= MAX_TREE_DEPTH {
            return self.report(
                location,
                ReadError::Path(text!("Достигнут предел вложенности папок").into()),
            );
        }
        let mut directory = match self.open_directory(location, stat) {
            Ok(directory) => directory,
            Err(error) => return self.report(location, error),
        };
        // Depth-first, with one handle per ancestor and no queue of siblings.
        loop {
            let (name, stat) = match self.request(|| next_sftp_entry(&mut directory)) {
                Ok(Some(entry)) => entry,
                Ok(None) => return true,
                Err(error) => return self.report(location, error),
            };
            if name == Path::new(".") || name == Path::new("..") {
                continue;
            }
            if !matches!(name.components().next(), Some(Component::Normal(part)) if part == name.as_os_str())
                || name.as_os_str().as_encoded_bytes().contains(&0)
            {
                self.report(
                    location,
                    ReadError::Path(text!("Папка содержит недопустимое имя").into()),
                );
                continue;
            }
            let child = location.join(name);
            (self.visit)(PropertyEvent::Entry {
                location: &child,
                stat: &stat,
                selected: false,
            });
            if stat.is_dir() && !self.scan_directory(&child, &stat, depth + 1) {
                return false;
            }
        }
    }
}

fn changed_directory() -> ReadError {
    ReadError::Path(text!("Папка изменилась во время подсчёта").into())
}

fn differs<T: PartialEq>(left: Option<T>, right: Option<T>) -> bool {
    left.zip(right).is_some_and(|(left, right)| left != right)
}
