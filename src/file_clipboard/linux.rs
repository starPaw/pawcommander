use super::{local_file_url, FileList};
use clipboard_rs::{Clipboard, ClipboardContent, ClipboardContext};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wl_clipboard_rs::copy::{MimeSource, MimeType as CopyMime, Options, Source};
use wl_clipboard_rs::paste::{self, ClipboardType, MimeType, Seat};

const URI_LIST: &str = "text/uri-list";
const GNOME_FILES: &str = "x-special/gnome-copied-files";
const KDE_CUT: &str = "application/x-kde-cutselection";
const REVISION: &str = "application/x-pawcommander-file-clipboard-revision";
const MAX_BYTES: usize = 4 * 1024 * 1024;

enum Backend {
    X11(ClipboardContext),
    Wayland,
}

impl Backend {
    fn connect() -> Result<Self, String> {
        if (std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("WAYLAND_SOCKET").is_some())
            && matches!(
                paste::get_mime_types(ClipboardType::Regular, Seat::Unspecified),
                Ok(_) | Err(paste::Error::ClipboardEmpty)
            )
        {
            return Ok(Self::Wayland);
        }
        ClipboardContext::new()
            .map(Self::X11)
            .map_err(|error| error.to_string())
    }

    fn formats(&self) -> Result<Vec<String>, String> {
        match self {
            Self::X11(clipboard) => clipboard
                .available_formats()
                .map_err(|error| error.to_string()),
            Self::Wayland => match paste::get_mime_types(ClipboardType::Regular, Seat::Unspecified)
            {
                Ok(formats) => Ok(formats.into_iter().collect()),
                Err(paste::Error::ClipboardEmpty) => Ok(Vec::new()),
                Err(error) => Err(error.to_string()),
            },
        }
    }

    fn read(&self, format: &str) -> Result<Vec<u8>, String> {
        let data = match self {
            Self::X11(clipboard) => clipboard
                .get_buffer(format)
                .map_err(|error| error.to_string())?,
            Self::Wayland => {
                let (pipe, _) = paste::get_contents(
                    ClipboardType::Regular,
                    Seat::Unspecified,
                    MimeType::Specific(format),
                )
                .map_err(|error| error.to_string())?;
                read_pipe(pipe)?
            }
        };
        if data.len() > MAX_BYTES {
            return Err(crate::i18n::text("Список файлов в буфере слишком велик").into());
        }
        Ok(data)
    }

    fn write(&self, formats: Vec<(&str, Vec<u8>)>) -> Result<(), String> {
        match self {
            Self::X11(clipboard) => clipboard
                .set(
                    formats
                        .into_iter()
                        .map(|(format, data)| ClipboardContent::Other(format.into(), data))
                        .collect(),
                )
                .map_err(|error| error.to_string()),
            Self::Wayland => Options::new()
                .copy_multi(
                    formats
                        .into_iter()
                        .map(|(format, data)| MimeSource {
                            source: Source::Bytes(data.into()),
                            mime_type: CopyMime::Specific(format.into()),
                        })
                        .collect(),
                )
                .map_err(|error| error.to_string()),
        }
    }
}

pub struct SystemClipboard {
    backend: Option<Backend>,
    write_count: u64,
}

impl SystemClipboard {
    pub fn new(_: &eframe::CreationContext<'_>) -> Self {
        Self {
            backend: None,
            write_count: 0,
        }
    }

    fn backend(&mut self) -> Result<&Backend, String> {
        if self.backend.is_none() {
            self.backend = Some(Backend::connect()?);
        }
        Ok(self.backend.as_ref().unwrap())
    }

    pub fn write(&mut self, paths: &[PathBuf], cut: bool) -> Result<(), String> {
        let uris = paths
            .iter()
            .map(|path| {
                url::Url::from_file_path(path)
                    .map(String::from)
                    .map_err(|_| {
                        crate::i18n::text("Путь нельзя записать в буфер как файловый URL")
                            .to_string()
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let uri_list = format!("{}\r\n", uris.join("\r\n"));
        let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
        if uri_list.len().max(gnome.len()) > MAX_BYTES {
            return Err(crate::i18n::text("Список файлов в буфере слишком велик").into());
        }
        self.write_count += 1;
        let clock = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let revision = format!("{}-{clock}-{}", std::process::id(), self.write_count);
        self.backend()?.write(vec![
            (URI_LIST, uri_list.into_bytes()),
            (GNOME_FILES, gnome.into_bytes()),
            (KDE_CUT, vec![if cut { b'1' } else { b'0' }]),
            (REVISION, revision.into_bytes()),
        ])
    }

    pub fn read(&mut self) -> Result<Option<FileList>, String> {
        let backend = self.backend()?;
        let formats = backend.formats()?;
        let has = |format: &str| formats.iter().any(|value| value == format);
        let revision = if has(REVISION) {
            Some(String::from_utf8(backend.read(REVISION)?).map_err(|error| error.to_string())?)
        } else {
            None
        };
        let (paths, cut) = if has(GNOME_FILES) {
            parse_gnome(&backend.read(GNOME_FILES)?)?
        } else if has(URI_LIST) {
            let payload = backend.read(URI_LIST)?;
            let cut = if has(KDE_CUT) {
                let cut = backend.read(KDE_CUT)? == b"1";
                if cut && backend.read(URI_LIST)? != payload {
                    return Err(
                        crate::i18n::text("Системный буфер изменился. Повторите вставку.").into(),
                    );
                }
                cut
            } else {
                false
            };
            (parse_uri_list(decode(&payload)?.lines())?, cut)
        } else {
            return Ok(None);
        };
        if let Some(revision) = &revision {
            if backend.read(REVISION)? != revision.as_bytes() {
                return Err(
                    crate::i18n::text("Системный буфер изменился. Повторите вставку.").into(),
                );
            }
        }
        FileList::new(paths, cut, revision)
    }
}

fn decode(data: &[u8]) -> Result<&str, String> {
    std::str::from_utf8(data)
        .map(|data| data.trim_end_matches('\0'))
        .map_err(|error| error.to_string())
}

fn parse_gnome(data: &[u8]) -> Result<(Vec<PathBuf>, bool), String> {
    let mut lines = decode(data)?.lines();
    let cut = match lines.next() {
        Some("cut") => true,
        Some("copy") => false,
        _ => return Err(crate::i18n::text("Неизвестный режим файлового буфера Linux").into()),
    };
    Ok((parse_uri_list(lines)?, cut))
}

fn parse_uri_list<'a>(lines: impl Iterator<Item = &'a str>) -> Result<Vec<PathBuf>, String> {
    lines
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(local_file_url)
        .collect()
}

fn read_pipe(mut pipe: impl Read + AsRawFd) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut fd = libc::pollfd {
        fd: pipe.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // The pipe is owned by this reader. poll may also return POLLHUP when the writer closes.
        let ready = unsafe { libc::poll(&mut fd, 1, remaining.as_millis() as i32) };
        if ready == 0 {
            return Err(crate::i18n::text("Владелец системного буфера не ответил вовремя").into());
        }
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.to_string());
        }
        let count = pipe.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            return Ok(data);
        }
        if data.len() + count > MAX_BYTES {
            return Err(crate::i18n::text("Список файлов в буфере слишком велик").into());
        }
        data.extend_from_slice(&buffer[..count]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_gnome_cut_and_uri_list() {
        let (paths, cut) = parse_gnome(b"cut\nfile:///home/test/a%20b\nfile:///tmp/x").unwrap();
        assert!(cut);
        assert_eq!(
            paths,
            [PathBuf::from("/home/test/a b"), PathBuf::from("/tmp/x")]
        );
        assert!(
            !parse_gnome(b"copy\r\n# comment\r\nfile:///tmp/z\r\n")
                .unwrap()
                .1
        );
        assert!(parse_gnome(b"move\nfile:///tmp/z").is_err());
        assert!(parse_gnome(b"cut\nfile:///tmp/z\nfile://nas/z").is_err());
    }
}
