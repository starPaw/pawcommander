use super::{decode_local_file_url, FileList, LocalFileUrlError};
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypeString};
use objc2_foundation::{NSArray, NSString, NSURL};
use std::collections::HashSet;
use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct SystemClipboard {
    cut_revision: Option<isize>,
    selection: Selection,
}

#[derive(Default)]
struct Selection {
    revision: Option<isize>,
    paths: Vec<(usize, PathBuf)>,
    consumed: HashSet<usize>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClipboardWriteError {
    PathContainsNul(PathBuf),
    Rejected,
    Changed,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClipboardReadError {
    Changed,
    Unreadable {
        revision: isize,
        cause: ClipboardReadFailure,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClipboardReadFailure {
    ItemsUnavailable,
    FileUrlUnavailable(usize),
    InvalidFileUrl {
        slot: usize,
        cause: LocalFileUrlError,
    },
    FoundationRejectedUrl(usize),
    UnresolvedReference {
        slot: usize,
        path: PathBuf,
    },
    InvalidPath(PathBuf),
}

impl SystemClipboard {
    pub fn new(_: &eframe::CreationContext<'_>) -> Self {
        Self::default()
    }

    pub fn read_text() -> Option<String> {
        autoreleasepool(|_| unsafe {
            NSPasteboard::generalPasteboard()
                .stringForType(NSPasteboardTypeString)
                .map(|text| text.to_string())
        })
    }

    pub(crate) fn write(
        &mut self,
        paths: &[PathBuf],
        cut: bool,
    ) -> Result<(), ClipboardWriteError> {
        autoreleasepool(|_| {
            let urls = paths
                .iter()
                .map(|path| {
                    let bytes = CString::new(path.as_os_str().as_bytes())
                        .map_err(|_| ClipboardWriteError::PathContainsNul(path.clone()))?;
                    let is_dir = path.symlink_metadata().is_ok_and(|meta| meta.is_dir());
                    // The file-system representation preserves symlinks and non-UTF-8 path bytes.
                    let url = unsafe {
                        NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(
                            std::ptr::NonNull::from(bytes.as_c_str()).cast(),
                            is_dir,
                            None,
                        )
                    };
                    Ok(ProtocolObject::from_retained(url))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let objects = NSArray::from_retained_slice(&urls);
            // AppKit owns the resulting URLs independently of PawCommander.
            let pasteboard = NSPasteboard::generalPasteboard();
            let revision = pasteboard.clearContents();
            let written = pasteboard.writeObjects(&objects);
            self.cut_revision = None;
            if pasteboard.changeCount() != revision {
                return Err(ClipboardWriteError::Changed);
            }
            if !written {
                return Err(ClipboardWriteError::Rejected);
            }
            // clearContents returns the revision we own. A later change count
            // could already belong to a selection written by another app.
            self.cut_revision = cut.then_some(revision);
            Ok(())
        })
    }

    pub(crate) fn read(&mut self) -> Result<Option<FileList>, ClipboardReadError> {
        autoreleasepool(|_| {
            let pasteboard = NSPasteboard::generalPasteboard();
            let revision = pasteboard.changeCount();
            let items = pasteboard.pasteboardItems();
            self.read_items(
                revision,
                items.as_ref().map(|items| items.iter()),
                |item, slot| {
                    let file_url_type = unsafe { NSPasteboardTypeFileURL };
                    if !item.types().containsObject(file_url_type) {
                        return Ok(None);
                    }
                    item.stringForType(file_url_type)
                        .map(Some)
                        .ok_or(ClipboardReadFailure::FileUrlUnavailable(slot))
                },
                || pasteboard.changeCount(),
            )
        })
    }

    fn read_items<T>(
        &mut self,
        revision: isize,
        items: Option<impl Iterator<Item = T>>,
        mut file_url: impl FnMut(T, usize) -> Result<Option<Retained<NSString>>, ClipboardReadFailure>,
        current_revision: impl FnOnce() -> isize,
    ) -> Result<Option<FileList>, ClipboardReadError> {
        let same_revision = self.selection.revision == Some(revision);
        let parsed = items
            .ok_or(ClipboardReadFailure::ItemsUnavailable)
            .and_then(|items| {
                let mut paths = Vec::new();
                for (slot, item) in items.enumerate() {
                    // Finder references can follow a move without a pasteboard change.
                    // Skip completed items by slot before resolving the remaining URLs.
                    if same_revision && self.selection.consumed.contains(&slot) {
                        continue;
                    }
                    if let Some(url) = file_url(item, slot)? {
                        paths.push((slot, clipboard_path(&url, slot)?));
                    }
                }
                Ok(paths)
            });
        if revision != current_revision() {
            return Err(ClipboardReadError::Changed);
        }
        let failure = |cause| ClipboardReadError::Unreadable { revision, cause };
        let paths = parsed.map_err(failure)?;
        let files = FileList::validated(
            paths.iter().map(|(_, path)| path.clone()).collect(),
            self.cut_revision == Some(revision),
            Some(revision.to_string()),
        )
        .map_err(|path| failure(ClipboardReadFailure::InvalidPath(path)))?;
        if !same_revision {
            self.selection.consumed.clear();
        }
        self.selection.revision = Some(revision);
        self.selection.paths = paths;
        Ok(files)
    }

    pub fn consume_moved(&mut self, files: &FileList, sources: &HashSet<&Path>) {
        let revision = files
            .revision
            .as_deref()
            .and_then(|value| value.parse().ok());
        if revision.is_none() || self.selection.revision != revision {
            return;
        }
        let Selection {
            paths, consumed, ..
        } = &mut self.selection;
        paths.retain(|(slot, path)| {
            if sources.contains(path.as_path()) {
                consumed.insert(*slot);
                false
            } else {
                true
            }
        });
    }
}

fn clipboard_path(value: &NSString, slot: usize) -> Result<PathBuf, ClipboardReadFailure> {
    // Keep the same local-only validation as the other Unix clipboard backend.
    let path = decode_local_file_url(&value.to_string())
        .map_err(|cause| ClipboardReadFailure::InvalidFileUrl { slot, cause })?;
    // Foundation copies/parses the live NSString. A Finder file-reference
    // URL identifies a resource, rather than a literal `/.file/id=...` path.
    let url =
        NSURL::URLWithString(value).ok_or(ClipboardReadFailure::FoundationRejectedUrl(slot))?;
    if !url.isFileReferenceURL() {
        // Ordinary paths must retain symlinks and need not currently exist.
        return Ok(path);
    }
    let resolved = url
        .filePathURL()
        .ok_or(ClipboardReadFailure::UnresolvedReference { slot, path })?;
    // SAFETY: the filesystem representation belongs to the live resolved NSURL;
    // copy its bytes before releasing the URL, preserving non-UTF-8 filenames.
    let bytes = unsafe { CStr::from_ptr(resolved.fileSystemRepresentation().as_ptr()) }.to_bytes();
    Ok(PathBuf::from(OsStr::from_bytes(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conflict::{Answer, Decision};
    use crate::fs_ops::TransferKind;
    use crate::operations::Progress;
    use crate::vfs::{self, Location};
    use std::os::unix::fs::symlink;

    fn native_url(path: &Path) -> Retained<NSURL> {
        let value = url::Url::from_file_path(path).unwrap();
        NSURL::URLWithString(&NSString::from_str(value.as_str())).unwrap()
    }

    fn reference_string(path: &Path) -> Retained<NSString> {
        let reference = native_url(path).fileReferenceURL().unwrap();
        assert!(reference.isFileReferenceURL());
        reference.absoluteString().unwrap()
    }

    #[test]
    fn retries_only_pending_finder_items_and_reresolves_their_paths() {
        autoreleasepool(|_| {
            let root = tempfile::tempdir_in("/private/tmp").unwrap();
            let folder = root.path().join("folder");
            let child = folder.join("child.txt");
            let pending = root.path().join("pending.txt");
            std::fs::create_dir(&folder).unwrap();
            std::fs::write(&child, b"child").unwrap();
            std::fs::write(&pending, b"pending").unwrap();

            let urls = [&folder, &child, &folder, &pending].map(|path| reference_string(path));
            let read = |clipboard: &mut SystemClipboard, revision, urls: &[Retained<NSString>]| {
                clipboard
                    .read_items(
                        revision,
                        Some(urls.iter()),
                        |url, _| Ok(Some(url.clone())),
                        || revision,
                    )
                    .unwrap()
            };
            let mut clipboard = SystemClipboard::default();
            let before = read(&mut clipboard, 1, &urls).unwrap();
            assert_eq!(
                before.paths,
                [&folder, &child, &pending].map(PathBuf::as_path)
            );

            let moved = root.path().join("moved");
            std::fs::rename(&folder, &moved).unwrap();
            clipboard.consume_moved(&before, &HashSet::from([folder.as_path(), child.as_path()]));
            let renamed = root.path().join("renamed.txt");
            std::fs::rename(&pending, &renamed).unwrap();
            let retry = read(&mut clipboard, 1, &urls).unwrap();
            assert_eq!(retry.revision, before.revision);
            assert_eq!(retry.paths, [renamed.as_path()]);

            std::fs::remove_dir_all(&moved).unwrap();
            assert_eq!(read(&mut clipboard, 1, &urls), Some(retry));

            let new_urls = [reference_string(&renamed)];
            let after = read(&mut clipboard, 2, &new_urls).unwrap();
            assert_ne!(after.revision, before.revision);
            assert_eq!(after.paths, [renamed.as_path()]);
            clipboard.consume_moved(&before, &HashSet::from([renamed.as_path()]));
            assert_eq!(read(&mut clipboard, 2, &new_urls), Some(after));
        });
    }

    #[test]
    fn plain_urls_retry_only_pending_paths_after_moving_a_directory_selected_through_aliases() {
        autoreleasepool(|_| {
            let root = tempfile::tempdir_in("/private/tmp").unwrap();
            let actual = root.path().join("actual");
            let alias = root.path().join("alias");
            let folder = actual.join("folder");
            let pending = alias.join("pending.txt");
            let target = root.path().join("target");
            let retry = root.path().join("retry");
            std::fs::create_dir_all(folder.join("nested")).unwrap();
            symlink(&actual, &alias).unwrap();
            std::fs::write(folder.join("nested/child.txt"), b"child").unwrap();
            std::fs::write(&pending, b"pending").unwrap();
            symlink(&pending, folder.join("link")).unwrap();
            std::fs::create_dir(&target).unwrap();
            std::fs::create_dir(&retry).unwrap();
            std::fs::write(target.join("pending.txt"), b"conflict").unwrap();

            let urls = [
                &alias.join("folder/nested/child.txt"),
                &alias.join("folder/nested"),
                &alias.join("folder/link"),
                &folder,
                &alias.join("folder"),
                &pending,
            ]
            .map(|path| native_url(path).absoluteString().unwrap());
            let read = |clipboard: &mut SystemClipboard| {
                clipboard
                    .read_items(1, Some(urls.iter()), |url, _| Ok(Some(url.clone())), || 1)
                    .unwrap()
                    .unwrap()
            };
            let mut clipboard = SystemClipboard::default();
            let before = read(&mut clipboard);
            let move_files = |clipboard: &mut SystemClipboard, files: &FileList, target: &Path| {
                let sources: Vec<Location> = files.paths.iter().cloned().map(Into::into).collect();
                vfs::transfer_with_conflicts(
                    &sources,
                    &target.to_path_buf().into(),
                    TransferKind::Move,
                    &mut Progress::default(),
                    |_| {
                        Ok(Answer {
                            decision: Decision::Skip,
                            apply_to_group: false,
                        })
                    },
                    |source, covered| {
                        let moved = std::iter::once(source.local().unwrap())
                            .chain(covered.iter().map(PathBuf::as_path))
                            .collect();
                        clipboard.consume_moved(files, &moved);
                    },
                )
                .unwrap()
            };
            let first = move_files(&mut clipboard, &before, &target);
            assert_eq!((first.completed, first.skipped), (1, 1));
            assert_eq!(
                std::fs::read(target.join("folder/nested/child.txt")).unwrap(),
                b"child"
            );
            assert_eq!(
                std::fs::read_link(target.join("folder/link")).unwrap(),
                pending
            );
            assert_eq!(
                std::fs::read(target.join("pending.txt")).unwrap(),
                b"conflict"
            );

            // Reusing the old pathname must not bring the completed aliases back.
            std::fs::create_dir_all(folder.join("nested")).unwrap();
            std::fs::write(folder.join("nested/child.txt"), b"new").unwrap();
            let remaining = read(&mut clipboard);
            assert_eq!(remaining.paths, [pending]);
            let second = move_files(&mut clipboard, &remaining, &retry);
            assert_eq!((second.completed, second.skipped), (1, 0));
            assert_eq!(
                std::fs::read(retry.join("pending.txt")).unwrap(),
                b"pending"
            );
            assert_eq!(
                std::fs::read(folder.join("nested/child.txt")).unwrap(),
                b"new"
            );
            assert!(!retry.join("folder").exists());
        });
    }

    #[test]
    fn resolves_finder_file_references_including_renamed_resources() {
        autoreleasepool(|_| {
            let root = tempfile::tempdir_in("/private/tmp").unwrap();
            let file = root.path().join("Привет #100% two words.txt");
            let directory = root.path().join("Folder #100%");
            std::fs::write(&file, b"clipboard").unwrap();
            std::fs::create_dir(&directory).unwrap();
            for path in [&file, &directory] {
                let reference = reference_string(path);
                assert_ne!(
                    decode_local_file_url(&reference.to_string()).unwrap(),
                    *path
                );
                assert_eq!(clipboard_path(&reference, 0).unwrap(), *path);
            }
            let reference = reference_string(&file);
            let renamed = root.path().join("renamed.txt");
            std::fs::rename(&file, &renamed).unwrap();
            assert_eq!(clipboard_path(&reference, 0).unwrap(), renamed);
        });
    }

    #[test]
    fn rejects_stale_finder_references_without_returning_synthetic_paths() {
        autoreleasepool(|_| {
            let root = tempfile::tempdir_in("/private/tmp").unwrap();
            let file = root.path().join("deleted.txt");
            std::fs::write(&file, b"clipboard").unwrap();
            let reference = reference_string(&file);
            std::fs::remove_file(&file).unwrap();
            let reference_path = decode_local_file_url(&reference.to_string()).unwrap();
            assert_eq!(
                clipboard_path(&reference, 2),
                Err(ClipboardReadFailure::UnresolvedReference {
                    slot: 2,
                    path: reference_path,
                })
            );
        });
    }

    #[test]
    fn ordinary_urls_preserve_symlinks_non_utf8_bytes_and_missing_paths() {
        autoreleasepool(|_| {
            let root = tempfile::tempdir_in("/private/tmp").unwrap();
            let file = root.path().join("file.txt");
            let link = root.path().join("link");
            let missing = root.path().join("missing");
            // APFS rejects creation of non-UTF-8 names, but decoding must still
            // preserve bytes in an ordinary URL without consulting the disk.
            let non_utf8 = root.path().join(OsStr::from_bytes(b"non-utf8-\xff"));
            std::fs::write(&file, b"clipboard").unwrap();
            symlink(&file, &link).unwrap();
            for path in [&file, &link, &missing, &non_utf8] {
                let url = native_url(path);
                assert!(!url.isFileReferenceURL());
                assert_eq!(
                    clipboard_path(&url.absoluteString().unwrap(), 0).unwrap(),
                    *path
                );
            }
            assert_eq!(std::fs::read_link(&link).unwrap(), file);
        });
    }

    #[test]
    fn rejects_nonlocal_and_malformed_clipboard_urls_before_native_resolution() {
        for value in [
            "file://nas/share/test",
            "file:///tmp/%00",
            "file:///tmp/a?query",
            "file:///tmp/a#fragment",
            "https://localhost/test",
            "not a URL",
        ] {
            assert!(
                clipboard_path(&NSString::from_str(value), 0).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn unreadable_file_and_revision_change_leave_the_last_selection_intact() {
        autoreleasepool(|_| {
            let mut clipboard = SystemClipboard::default();
            let first = NSString::from_str("file:///private/tmp/first.txt");
            let read = |clipboard: &mut SystemClipboard, revision, current_revision| {
                clipboard.read_items(
                    revision,
                    Some([true, false].into_iter()),
                    |has_url, slot| {
                        if has_url {
                            Ok(Some(first.clone()))
                        } else {
                            Err(ClipboardReadFailure::FileUrlUnavailable(slot))
                        }
                    },
                    || current_revision,
                )
            };
            let previous = clipboard
                .read_items(
                    1,
                    Some([NSString::from_str("file:///private/tmp/previous.txt")].into_iter()),
                    |url, _| Ok(Some(url)),
                    || 1,
                )
                .unwrap()
                .unwrap();

            assert_eq!(
                read(&mut clipboard, 2, 2),
                Err(ClipboardReadError::Unreadable {
                    revision: 2,
                    cause: ClipboardReadFailure::FileUrlUnavailable(1),
                })
            );
            assert_eq!(read(&mut clipboard, 2, 3), Err(ClipboardReadError::Changed));
            assert_eq!(clipboard.selection.revision, Some(1));
            assert_eq!(clipboard.selection.paths[0].1, previous.paths[0]);
            assert_eq!(clipboard.selection.paths.len(), 1);
        });
    }

    #[test]
    fn empty_clipboard_and_unavailable_items_are_distinct() {
        let mut clipboard = SystemClipboard::default();
        let file_url = |(), _| unreachable!("the snapshot has no items");
        assert_eq!(
            clipboard.read_items(1, None::<std::iter::Empty<()>>, file_url, || 1),
            Err(ClipboardReadError::Unreadable {
                revision: 1,
                cause: ClipboardReadFailure::ItemsUnavailable,
            })
        );
        assert_eq!(
            clipboard.read_items(1, Some(std::iter::empty()), file_url, || 1),
            Ok(None)
        );
    }
}
