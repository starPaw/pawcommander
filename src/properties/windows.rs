//! Handle-relative, read-only traversal. Paths are diagnostic after root acquisition.
use super::Summary;
use std::ffi::{c_void, OsString};
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES;
use windows_sys::Win32::Storage::FileSystem::{
    FileIdExtdDirectoryInfo, FileIdInfo, GetFileInformationByHandleEx, FILE_ATTRIBUTE_DEVICE,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, SYNCHRONIZE,
};

pub(super) fn open_root(path: &Path) -> io::Result<(File, Metadata)> {
    // Query metadata through this same no-follow handle, rather than inspecting
    // a path and subsequently opening whatever has replaced it.
    let root = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = root.metadata()?;
    Ok((root, metadata))
}

pub(super) fn scan_root(
    mut path: PathBuf,
    root: File,
    metadata: Metadata,
    cancel: &AtomicBool,
    summary: &mut Summary,
    progress: &mut impl FnMut(&Summary),
) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    let attributes = metadata.file_attributes();
    count_entry(summary, attributes, metadata.len());
    if !is_directory(attributes) || cancel.load(Ordering::Relaxed) {
        progress(summary);
        return;
    }
    // Obtain enumeration access relative to the held root. No root pathname is
    // reopened. An empty NT relative name reopens the held object; unlike
    // Win32 paths, literal '.' is not normalized by NtCreateFile.
    let open_root_directory = || {
        let expected = identity(&root)?;
        let directory = open_directory(&root, &[])?;
        if identity(&directory)? != expected {
            return Err(changed_directory());
        }
        Directory::new(directory)
    };
    let root = match open_root_directory() {
        Ok(root) => root,
        Err(error) => {
            summary.error(&path, error);
            progress(summary);
            return;
        }
    };
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
        if is_directory(entry.attributes) {
            progress(summary);
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            path.push(OsString::from_wide(&entry.name));
            match directory.descend(&entry) {
                Ok(child) => {
                    summary.directories += 1;
                    directories.push(child);
                }
                Err(error) => {
                    summary.error(&path, error);
                    path.pop();
                }
            }
        } else {
            count_entry(summary, entry.attributes, entry.bytes);
        }
        progress(summary);
    }
}

fn is_directory(attributes: u32) -> bool {
    attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
        == FILE_ATTRIBUTE_DIRECTORY
}

fn count_entry(summary: &mut Summary, attributes: u32, bytes: u64) {
    // Treat all reparse points conservatively, including junctions, mount points
    // and filter-managed entries: none may redirect traversal outside this tree.
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        summary.links += 1;
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        summary.directories += 1;
    } else if attributes & FILE_ATTRIBUTE_DEVICE != 0 {
        summary.other += 1;
    } else {
        summary.files += 1;
        summary.bytes = summary.bytes.saturating_add(bytes);
    }
}

struct Directory {
    file: File,
    volume: u64,
    // u64 storage satisfies the API's eight-byte buffer alignment requirement.
    buffer: Vec<u64>,
    next: Option<usize>,
}

struct Entry {
    name: Vec<u16>,
    attributes: u32,
    bytes: u64,
    id: [u8; 16],
}

impl Directory {
    fn new(file: File) -> io::Result<Self> {
        if !is_directory(file.metadata()?.file_attributes()) {
            return Err(changed_directory());
        }
        let (volume, _) = identity(&file)?;
        Ok(Self {
            file,
            volume,
            buffer: vec![0; 1024],
            next: None,
        })
    }

    fn next(&mut self) -> io::Result<Option<Entry>> {
        loop {
            if self.next.is_none() {
                self.buffer.fill(0);
                // SAFETY: file owns a live directory handle; buffer is aligned,
                // writable, and its supplied size matches its allocation.
                let result = unsafe {
                    GetFileInformationByHandleEx(
                        self.file.as_raw_handle(),
                        FileIdExtdDirectoryInfo,
                        self.buffer.as_mut_ptr().cast(),
                        (self.buffer.len() * size_of::<u64>()) as u32,
                    )
                };
                if result == 0 {
                    let error = io::Error::last_os_error();
                    return if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                        Ok(None)
                    } else {
                        Err(error)
                    };
                }
                self.next = Some(0);
            }
            let offset = self.next.unwrap();
            let capacity = self.buffer.len() * size_of::<u64>();
            if offset > capacity.saturating_sub(size_of::<FILE_ID_EXTD_DIR_INFO>()) {
                return Err(invalid_directory_data());
            }
            // SAFETY: checked the complete fixed struct fits in initialized
            // storage. Copy it; variable filename data is bounded separately.
            let info = unsafe {
                self.buffer
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset)
                    .cast::<FILE_ID_EXTD_DIR_INFO>()
                    .read_unaligned()
            };
            let name_start = offset + offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
            let name_bytes = info.FileNameLength as usize;
            if name_bytes == 0
                || !name_bytes.is_multiple_of(2)
                || name_bytes > capacity - name_start
            {
                return Err(invalid_directory_data());
            }
            if info.NextEntryOffset == 0 {
                self.next = None;
            } else {
                let step = info.NextEntryOffset as usize;
                if !step.is_multiple_of(8)
                    || step < name_start - offset + name_bytes
                    || step > capacity - offset
                {
                    return Err(invalid_directory_data());
                }
                self.next = Some(offset + step);
            }
            // SAFETY: name range was checked above, and UTF-16 alignment follows
            // from the eight-byte entry alignment and the struct field offset.
            let name = unsafe {
                std::slice::from_raw_parts(
                    self.buffer
                        .as_ptr()
                        .cast::<u8>()
                        .add(name_start)
                        .cast::<u16>(),
                    name_bytes / 2,
                )
            };
            if name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16] {
                continue;
            }
            // Never allow enumeration output to become a multi-component NT
            // pathname, alternate stream, or absolute name.
            if !valid_child_name(name) {
                return Err(invalid_directory_data());
            }
            return Ok(Some(Entry {
                name: name.to_vec(),
                attributes: info.FileAttributes,
                bytes: info.EndOfFile.max(0) as u64,
                id: info.FileId.Identifier,
            }));
        }
    }

    fn descend(&self, entry: &Entry) -> io::Result<Self> {
        if !valid_child_name(&entry.name) {
            return Err(invalid_directory_data());
        }
        if !valid_id(entry.id) {
            return Err(changed_directory());
        }
        let file = open_directory(&self.file, &entry.name)?;
        if identity(&file)? != (self.volume, entry.id)
            || !is_directory(file.metadata()?.file_attributes())
        {
            return Err(changed_directory());
        }
        Self::new(file)
    }
}

fn valid_child_name(name: &[u16]) -> bool {
    !name.is_empty()
        && name != [b'.' as u16]
        && name != [b'.' as u16, b'.' as u16]
        && !name.iter().any(|unit| matches!(*unit, 0 | 47 | 58 | 92))
}

fn valid_id(id: [u8; 16]) -> bool {
    id != [0; 16] && id != [u8::MAX; 16]
}

fn identity(file: &File) -> io::Result<(u64, [u8; 16])> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: file owns a live handle, and the writable output matches FileIdInfo.
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    if !valid_id(info.FileId.Identifier) {
        return Err(changed_directory());
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

fn invalid_directory_data() -> io::Error {
    io::Error::from(io::ErrorKind::InvalidData)
}

fn changed_directory() -> io::Error {
    io::Error::other(crate::i18n::text!("Папка изменилась во время подсчёта"))
}

// Windows SDK winternl.h/ntdef.h layouts. Keep this private FFI here instead of
// enabling the much larger WDK API surface in the application's dependencies.
// https://learn.microsoft.com/windows/win32/api/winternl/nf-winternl-ntcreatefile
#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: *mut c_void,
    object_name: *const UnicodeString,
    attributes: u32,
    security_descriptor: *mut c_void,
    security_quality_of_service: *mut c_void,
}

#[repr(C)]
struct IoStatusBlock {
    // The first SDK field is a union of NTSTATUS and PVOID, with pointer size
    // and alignment. NtCreateFile initializes it; Rust never reads the union.
    status_or_pointer: usize,
    information: usize,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    #[link_name = "NtCreateFile"]
    fn nt_create_file(
        handle: *mut *mut c_void,
        access: u32,
        attributes: *const ObjectAttributes,
        status: *mut IoStatusBlock,
        allocation_size: *const i64,
        file_attributes: u32,
        share_access: u32,
        disposition: u32,
        options: u32,
        ea_buffer: *const c_void,
        ea_length: u32,
    ) -> i32;
    #[link_name = "RtlNtStatusToDosError"]
    fn nt_status_to_dos_error(status: i32) -> u32;
}

fn open_directory(parent: &File, name: &[u16]) -> io::Result<File> {
    let length = name
        .len()
        .checked_mul(2)
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(invalid_directory_data)?;
    // Keep even the empty name backed by readable storage for the native API.
    let empty_name = [0u16];
    let name = UnicodeString {
        length,
        maximum_length: if name.is_empty() { 2 } else { length },
        buffer: if name.is_empty() {
            empty_name.as_ptr()
        } else {
            name.as_ptr()
        },
    };
    let attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory: parent.as_raw_handle(),
        object_name: &name,
        // Preserve exact enumerated case (also for case-sensitive directories).
        // OBJ_DONT_REPARSE refuses reparsing anywhere in this relative lookup.
        attributes: 0x1000,
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut status = IoStatusBlock {
        status_or_pointer: 0,
        information: 0,
    };
    let mut handle = std::ptr::null_mut();
    // SAFETY: parent is live; name/attributes/status/handle remain valid for
    // this synchronous call. FILE_OPEN cannot create, truncate or write a file.
    let result = unsafe {
        nt_create_file(
            &mut handle,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            &attributes,
            &mut status,
            std::ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            1,                        // FILE_OPEN
            0x1 | 0x20 | 0x0020_0000, // DIRECTORY_FILE | SYNCHRONOUS_IO_NONALERT | OPEN_REPARSE_POINT
            std::ptr::null(),
            0,
        )
    };
    if result < 0 {
        // SAFETY: accepts any NTSTATUS value and has no pointer arguments.
        return Err(io::Error::from_raw_os_error(unsafe {
            nt_status_to_dos_error(result) as i32
        }));
    }
    // SAFETY: successful synchronous NtCreateFile returned a new owned handle.
    Ok(unsafe { File::from_raw_handle(handle) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::properties::scan;
    use std::fs;

    fn junction(link: &Path, target: &Path) {
        // Disposable test fixtures only. Junctions do not require the symbolic
        // link privilege or Developer Mode. Both names belong to this tempdir.
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("run mklink for disposable junction");
        assert!(
            output.status.success(),
            "junction fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn ordinary_root_is_enumerated_from_its_handle() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("file"), b"yes").unwrap();
        let (root, metadata) = open_root(temp.path()).unwrap();
        let mut summary = Summary::default();
        scan_root(
            temp.path().into(),
            root,
            metadata,
            &AtomicBool::new(false),
            &mut summary,
            &mut |_| {},
        );
        assert_eq!(
            (summary.files, summary.directories, summary.bytes),
            (1, 1, 3)
        );
        assert_eq!(summary.errors, 0);
    }

    #[test]
    fn held_root_reopens_after_its_path_is_replaced_by_junction() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir(&root_path).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root_path.join("file"), b"yes").unwrap();
        fs::write(outside.join("file"), [0; 101]).unwrap();
        let (root, metadata) = open_root(&root_path).unwrap();
        fs::rename(&root_path, temp.path().join("moved")).unwrap();
        junction(&root_path, &outside);
        let mut summary = Summary::default();
        scan_root(
            root_path,
            root,
            metadata,
            &AtomicBool::new(false),
            &mut summary,
            &mut |_| {},
        );
        assert_eq!(
            (summary.files, summary.directories, summary.bytes),
            (1, 1, 3)
        );
        assert_eq!(summary.errors, 0);
    }

    #[test]
    fn child_names_cannot_reopen_root_or_escape_parent() {
        for name in [
            "",
            ".",
            "..",
            "\\outside",
            "../outside",
            "file:stream",
            "a\0b",
        ] {
            assert!(!valid_child_name(&name.encode_utf16().collect::<Vec<_>>()));
        }
        assert!(valid_child_name(
            &"child".encode_utf16().collect::<Vec<_>>()
        ));
    }

    #[test]
    fn junction_and_its_external_target_are_not_traversed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root.join("file"), b"yes").unwrap();
        fs::write(outside.join("file"), [0; 101]).unwrap();
        let link = root.join("junction");
        junction(&link, &outside);
        let result = scan(&[root], &AtomicBool::new(false), |_| {});
        assert_eq!(
            (result.files, result.directories, result.links, result.bytes),
            (1, 1, 1, 3)
        );
        assert_eq!(result.errors, 0);
        let result = scan(&[link], &AtomicBool::new(false), |_| {});
        assert_eq!(
            (result.files, result.directories, result.links, result.bytes),
            (0, 0, 1, 0)
        );
        assert_eq!(result.errors, 0);
    }

    #[test]
    fn replacing_an_open_ancestor_with_junction_keeps_original_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let ancestor = root.join("ancestor");
        let outside = temp.path().join("outside");
        fs::create_dir_all(ancestor.join("nested")).unwrap();
        fs::create_dir_all(outside.join("nested")).unwrap();
        fs::write(ancestor.join("nested/file"), b"yes").unwrap();
        fs::write(outside.join("nested/file"), [0; 101]).unwrap();
        let mut ancestor_updates = 0;
        let result = scan(&[root], &AtomicBool::new(false), |summary| {
            if summary.directories == 2 {
                ancestor_updates += 1;
                if ancestor_updates == 2 {
                    fs::rename(&ancestor, temp.path().join("moved")).unwrap();
                    junction(&ancestor, &outside);
                }
            }
        });
        assert_eq!(ancestor_updates, 2);
        assert_eq!(
            (result.files, result.directories, result.links, result.bytes),
            (1, 3, 0, 3)
        );
        assert_eq!(result.errors, 0);
    }

    #[test]
    fn child_replaced_by_junction_reports_partial_without_external_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let child = root.join("child");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("file"), [0; 101]).unwrap();
        let mut updates = 0;
        let mut replaced = false;
        let result = scan(&[root], &AtomicBool::new(false), |_| {
            updates += 1;
            if updates == 2 {
                fs::rename(&child, temp.path().join("moved")).unwrap();
                junction(&child, &outside);
                replaced = true;
            }
        });
        assert!(
            replaced,
            "the child junction replacement must have occurred"
        );
        assert_eq!((result.files, result.directories, result.bytes), (0, 1, 0));
        assert_eq!(result.errors, 1);
        assert!(result.error_examples[0].contains(&child.display().to_string()));
        assert!(result.done && !result.cancelled);
    }

    #[test]
    fn child_replaced_by_other_directory_reports_identity_change() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let child = root.join("child");
        fs::create_dir_all(&child).unwrap();
        let mut updates = 0;
        let result = scan(&[root], &AtomicBool::new(false), |_| {
            updates += 1;
            if updates == 2 {
                fs::rename(&child, temp.path().join("moved")).unwrap();
                fs::create_dir(&child).unwrap();
                fs::write(child.join("file"), b"replacement").unwrap();
            }
        });
        assert_eq!((result.files, result.directories, result.bytes), (0, 1, 0));
        assert_eq!(result.errors, 1);
        assert!(result.error_examples[0].contains(&child.display().to_string()));
    }

    #[test]
    fn cancellation_after_observing_subdirectory_skips_descent() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        fs::write(child.join("file"), b"data").unwrap();
        let cancel = AtomicBool::new(false);
        let mut updates = 0;
        let result = scan(&[temp.path().into()], &cancel, |_| {
            updates += 1;
            if updates == 2 {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert!(result.done && result.cancelled);
        assert_eq!((result.files, result.directories, result.bytes), (0, 1, 0));
        assert_eq!(result.errors, 0);
    }
}
