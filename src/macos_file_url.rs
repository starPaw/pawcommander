use objc2::rc::Retained;
use objc2_foundation::NSURL;
use std::{
    ffi::{CStr, CString, NulError, OsStr},
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub(crate) enum PathError {
    ContainsNul(NulError),
    Unrepresentable,
}

impl From<PathError> for io::Error {
    fn from(error: PathError) -> Self {
        match error {
            PathError::ContainsNul(source) => Self::new(io::ErrorKind::InvalidInput, source),
            PathError::Unrepresentable => io::ErrorKind::InvalidInput.into(),
        }
    }
}

/// Use the caller's directory flag without looking up or resolving the path.
pub(crate) fn from_path(path: &Path, is_directory: bool) -> Result<Retained<NSURL>, PathError> {
    NSURL::from_path(path, is_directory, None).ok_or_else(|| {
        // The binding reports invalid paths as None. Recover the NUL position
        // only on failure for callers that report the underlying cause.
        match CString::new(path.as_os_str().as_bytes()) {
            Err(source) => PathError::ContainsNul(source),
            Ok(_) => PathError::Unrepresentable,
        }
    })
}

pub(crate) fn to_path(url: &NSURL) -> Option<PathBuf> {
    if !url.isFileURL() {
        return None;
    }
    // Panels and resolved Finder references can supply paths longer than the
    // fixed buffer used by NSURL::to_file_path for file promise destinations.
    // SAFETY: the NUL-terminated representation is borrowed only while the URL is live.
    let bytes = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) };
    Some(PathBuf::from(OsStr::from_bytes(bytes.to_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_urls_preserve_path_bytes_and_the_supplied_directory_flag() {
        let path = Path::new(OsStr::from_bytes(b"/private/tmp/uncreated # % \xff"));
        for directory in [false, true] {
            let url = from_path(path, directory).unwrap();
            assert_eq!(url.hasDirectoryPath(), directory);
            assert_eq!(to_path(&url).as_deref(), Some(path));
        }
    }

    #[test]
    fn reads_long_file_paths_and_rejects_other_schemes() {
        let path = Path::new("/private/tmp")
            .join("component/".repeat(120))
            .join("file");
        let url = from_path(&path, false).unwrap();
        assert_eq!(to_path(&url).unwrap(), path);
        let web = NSURL::URLWithString(&objc2_foundation::NSString::from_str(
            "https://example.invalid/file",
        ))
        .unwrap();
        assert!(to_path(&web).is_none());
    }

    #[test]
    fn invalid_paths_preserve_the_nul_error_and_reject_an_empty_path() {
        let error = io::Error::from(from_path(Path::new("a\0b"), false).unwrap_err());
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let cause = error.get_ref().unwrap().downcast_ref::<NulError>().unwrap();
        assert_eq!(cause.nul_position(), 1);
        assert!(matches!(
            from_path(Path::new(""), false),
            Err(PathError::Unrepresentable)
        ));
    }
}
