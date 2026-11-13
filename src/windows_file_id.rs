//! Stable Windows file identity from an open handle, including directories.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::{
    FileIdInfo, GetFileInformationByHandleEx, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WindowsFileId {
    pub(crate) volume: u64,
    pub(crate) id: [u8; 16],
}

pub(crate) fn from_handle(file: &File) -> io::Result<Option<WindowsFileId>> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the live handle and output buffer match the FileIdInfo class.
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
    let id = info.FileId.Identifier;
    if id == [0; 16] || id == [u8::MAX; 16] {
        return Ok(None);
    }
    Ok(Some(WindowsFileId {
        volume: info.VolumeSerialNumber,
        id,
    }))
}

pub(crate) fn from_path(path: &Path) -> io::Result<Option<WindowsFileId>> {
    use std::os::windows::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(None);
    }
    let file = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    // Recheck the handle after opening to avoid following a swapped reparse point.
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(None);
    }
    from_handle(&file)
}
