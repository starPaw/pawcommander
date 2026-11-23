use std::collections::HashSet;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::SystemClipboard;
#[cfg(target_os = "macos")]
pub use macos::SystemClipboard;
#[cfg(target_os = "macos")]
pub(crate) use macos::{
    ClipboardReadError, ClipboardReadFailure, ClipboardWriteError, PasteboardMove,
};
#[cfg(target_os = "windows")]
pub use windows::SystemClipboard;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileList {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
    pub revision: Option<String>,
}

impl FileList {
    #[cfg(target_os = "macos")]
    pub(crate) fn pasteboard_revision(&self) -> Option<isize> {
        self.revision.as_deref()?.parse().ok()
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn new(
        paths: Vec<PathBuf>,
        cut: bool,
        revision: Option<String>,
    ) -> Result<Option<Self>, String> {
        Self::validated(paths, cut, revision).map_err(|_| {
            crate::i18n::text!("Файловый буфер содержит путь без имени или относительный путь")
                .into()
        })
    }

    fn validated(
        mut paths: Vec<PathBuf>,
        cut: bool,
        revision: Option<String>,
    ) -> Result<Option<Self>, PathBuf> {
        if let Some(invalid) = paths
            .iter()
            .find(|path| !path.is_absolute() || path.file_name().is_none())
        {
            return Err(invalid.clone());
        }
        let mut seen = HashSet::new();
        paths.retain(|path| seen.insert(path.clone()));
        Ok((!paths.is_empty()).then_some(Self {
            paths,
            cut,
            revision,
        }))
    }
}

#[cfg(target_os = "linux")]
fn local_file_url(uri: &str) -> Result<PathBuf, String> {
    decode_local_file_url(uri).map_err(|_| {
        crate::i18n::text!("Файловый буфер содержит недопустимый или нелокальный URL").to_string()
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LocalFileUrlError {
    Parse(url::ParseError),
    Unsupported,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn decode_local_file_url(uri: &str) -> Result<PathBuf, LocalFileUrlError> {
    let url = url::Url::parse(uri).map_err(LocalFileUrlError::Parse)?;
    if url.scheme() != "file"
        || url.host_str().is_some_and(|host| host != "localhost")
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path().contains("%00")
    {
        return Err(LocalFileUrlError::Unsupported);
    }
    url.to_file_path()
        .map_err(|()| LocalFileUrlError::Unsupported)
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub struct SystemClipboard;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
impl SystemClipboard {
    pub fn new(_: &eframe::CreationContext<'_>) -> Self {
        Self
    }

    pub fn write(&mut self, _: &[PathBuf], _: bool) -> Result<(), String> {
        Err(crate::i18n::text!("Системный файловый буфер недоступен на этой платформе").into())
    }

    pub fn read(&mut self) -> Result<Option<FileList>, String> {
        Ok(None)
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn decodes_local_file_urls_only() {
        assert_eq!(
            decode_local_file_url("file:///tmp/%D1%84%20%25%23%0A").unwrap(),
            PathBuf::from("/tmp/ф %#\n")
        );
        for uri in [
            "file://nas/share/test",
            "file:///tmp/%00",
            "file:///tmp/a?foo",
            "https://localhost/test",
        ] {
            assert!(decode_local_file_url(uri).is_err(), "{uri}");
        }
    }
}
