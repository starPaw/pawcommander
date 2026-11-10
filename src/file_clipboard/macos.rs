use super::{local_file_url, FileList};
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL};
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

impl SystemClipboard {
    pub fn new(_: &eframe::CreationContext<'_>) -> Self {
        Self::default()
    }

    pub fn write(&mut self, paths: &[PathBuf], cut: bool) -> Result<(), String> {
        autoreleasepool(|_| {
            let urls = paths
                .iter()
                .map(|path| {
                    let bytes = CString::new(path.as_os_str().as_bytes())
                        .map_err(|_| crate::i18n::text("Путь содержит нулевой байт").to_string())?;
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
                .collect::<Result<Vec<_>, String>>()?;
            let objects = NSArray::from_vec(urls);
            // AppKit owns the resulting URLs independently of PawCommander.
            let pasteboard = unsafe { NSPasteboard::generalPasteboard() };
            unsafe { pasteboard.clearContents() };
            if !unsafe { pasteboard.writeObjects(&objects) } {
                return Err(crate::i18n::text("Не удалось записать файлы в буфер Finder").into());
            }
            self.cut_revision = cut.then(|| unsafe { pasteboard.changeCount() });
            Ok(())
        })
    }

    pub fn read(&mut self) -> Result<Option<FileList>, String> {
        autoreleasepool(|_| {
            let pasteboard = unsafe { NSPasteboard::generalPasteboard() };
            let revision = unsafe { pasteboard.changeCount() };
            let items = unsafe { pasteboard.pasteboardItems() };
            self.read_items(
                revision,
                items.iter().flat_map(|items| items.iter()),
                |item| unsafe { item.stringForType(NSPasteboardTypeFileURL) },
                || unsafe { pasteboard.changeCount() },
            )
        })
    }

    fn read_items<T>(
        &mut self,
        revision: isize,
        items: impl Iterator<Item = T>,
        mut file_url: impl FnMut(T) -> Option<Retained<NSString>>,
        current_revision: impl FnOnce() -> isize,
    ) -> Result<Option<FileList>, String> {
        let same_revision = self.selection.revision == Some(revision);
        let mut paths = Vec::new();
        for (slot, item) in items.enumerate() {
            // Finder references can follow a move without a pasteboard change.
            // Skip completed items by slot before resolving the remaining URLs.
            if same_revision && self.selection.consumed.contains(&slot) {
                continue;
            }
            if let Some(url) = file_url(item) {
                paths.push((slot, clipboard_path(&url)?));
            }
        }
        if revision != current_revision() {
            return Err(crate::i18n::text("Системный буфер изменился. Повторите вставку.").into());
        }
        let files = FileList::new(
            paths.iter().map(|(_, path)| path.clone()).collect(),
            self.cut_revision == Some(revision),
            Some(revision.to_string()),
        )?;
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

fn clipboard_path(value: &NSString) -> Result<PathBuf, String> {
    // Keep the same local-only validation as the other Unix clipboard backend.
    let path = local_file_url(&value.to_string())?;
    // SAFETY: Foundation copies/parses the live NSString. A Finder file-reference
    // URL identifies a resource, rather than a literal `/.file/id=...` path.
    let url = unsafe { NSURL::URLWithString(value) }.ok_or(crate::i18n::text(
        "Файловый буфер содержит недопустимый URL",
    ))?;
    if !unsafe { url.isFileReferenceURL() } {
        // Ordinary paths must retain symlinks and need not currently exist.
        return Ok(path);
    }
    let resolved = unsafe { url.filePathURL() }.ok_or(crate::i18n::text(
        "Не удалось найти файл из буфера Finder: ссылка устарела или файл недоступен",
    ))?;
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
        unsafe { NSURL::URLWithString(&NSString::from_str(value.as_str())) }.unwrap()
    }

    fn reference_string(path: &Path) -> Retained<NSString> {
        let reference = unsafe { native_url(path).fileReferenceURL() }.unwrap();
        assert!(unsafe { reference.isFileReferenceURL() });
        unsafe { reference.absoluteString() }.unwrap()
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
                    .read_items(revision, urls.iter(), |url| Some(url.clone()), || revision)
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
            .map(|path| unsafe { native_url(path).absoluteString() }.unwrap());
            let read = |clipboard: &mut SystemClipboard| {
                clipboard
                    .read_items(1, urls.iter(), |url| Some(url.clone()), || 1)
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
                assert_ne!(local_file_url(&reference.to_string()).unwrap(), *path);
                assert_eq!(clipboard_path(&reference).unwrap(), *path);
            }
            let reference = reference_string(&file);
            let renamed = root.path().join("renamed.txt");
            std::fs::rename(&file, &renamed).unwrap();
            assert_eq!(clipboard_path(&reference).unwrap(), renamed);
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
            assert!(clipboard_path(&reference).is_err());
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
                assert!(!unsafe { url.isFileReferenceURL() });
                assert_eq!(
                    clipboard_path(&unsafe { url.absoluteString() }.unwrap()).unwrap(),
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
                clipboard_path(&NSString::from_str(value)).is_err(),
                "{value}"
            );
        }
    }
}
