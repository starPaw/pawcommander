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
#[cfg(target_os = "windows")]
pub use windows::SystemClipboard;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileList {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
    pub revision: Option<String>,
}

impl FileList {
    fn new(
        mut paths: Vec<PathBuf>,
        cut: bool,
        revision: Option<String>,
    ) -> Result<Option<Self>, String> {
        let mut seen = HashSet::new();
        if paths
            .iter()
            .any(|path| !path.is_absolute() || path.file_name().is_none())
        {
            return Err(crate::i18n::text(
                "Файловый буфер содержит путь без имени или относительный путь",
            )
            .into());
        }
        paths.retain(|path| seen.insert(path.clone()));
        Ok((!paths.is_empty()).then_some(Self {
            paths,
            cut,
            revision,
        }))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn local_file_url(uri: &str) -> Result<PathBuf, String> {
    let invalid = || {
        crate::i18n::text("Файловый буфер содержит недопустимый или нелокальный URL").to_string()
    };
    let url = url::Url::parse(uri).map_err(|_| invalid())?;
    if url.scheme() != "file"
        || url.host_str().is_some_and(|host| host != "localhost")
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path().contains("%00")
    {
        return Err(invalid());
    }
    url.to_file_path().map_err(|_| invalid())
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub struct SystemClipboard;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
impl SystemClipboard {
    pub fn new(_: &eframe::CreationContext<'_>) -> Self {
        Self
    }

    pub fn write(&mut self, _: &[PathBuf], _: bool) -> Result<(), String> {
        Err(crate::i18n::text("Системный файловый буфер недоступен на этой платформе").into())
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
            local_file_url("file:///tmp/%D1%84%20%25%23%0A").unwrap(),
            PathBuf::from("/tmp/ф %#\n")
        );
        for uri in [
            "file://nas/share/test",
            "file:///tmp/%00",
            "file:///tmp/a?foo",
            "https://localhost/test",
        ] {
            assert!(local_file_url(uri).is_err(), "{uri}");
        }
    }
}
