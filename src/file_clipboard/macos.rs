use super::{local_file_url, FileList};
use objc2::rc::autoreleasepool;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL};
use objc2_foundation::{NSArray, NSString, NSURL};
use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

pub struct SystemClipboard {
    cut_revision: Option<isize>,
}

impl SystemClipboard {
    pub fn new(_: &eframe::CreationContext<'_>) -> Self {
        Self { cut_revision: None }
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
            let mut paths = Vec::new();
            if let Some(items) = unsafe { pasteboard.pasteboardItems() } {
                for item in items.iter() {
                    if let Some(url) = unsafe { item.stringForType(NSPasteboardTypeFileURL) } {
                        paths.push(clipboard_path(&url)?);
                    }
                }
            }
            if revision != unsafe { pasteboard.changeCount() } {
                return Err(
                    crate::i18n::text("Системный буфер изменился. Повторите вставку.").into(),
                );
            }
            FileList::new(
                paths,
                self.cut_revision == Some(revision),
                Some(revision.to_string()),
            )
        })
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
    use objc2::rc::Retained;
    use std::os::unix::fs::symlink;
    use std::path::Path;

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
