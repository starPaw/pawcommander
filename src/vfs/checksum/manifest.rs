use crate::{
    file_error::{ChecksumError, ManifestLineError},
    operations::{Cancellation, TransferError},
    vfs::Location,
};
use std::{
    collections::HashSet,
    io::Write,
    path::{Component, Path, PathBuf},
};

pub(super) const MAX_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_ENTRIES: usize = 50_000;
const MAX_PATH_BYTES: usize = 4096;
const MAX_DEPTH: usize = 128;

#[derive(Debug)]
pub(super) struct Entry {
    pub(super) path: PathBuf,
    pub(super) digest: [u8; 32],
}

pub(super) fn parse(
    bytes: &[u8],
    manifest: &Location,
    cancel: &Cancellation,
) -> Result<Vec<Entry>, TransferError> {
    if bytes.len() > MAX_BYTES {
        return Err(ChecksumError::TooLarge(manifest.clone()).into());
    }
    let mut entries = Vec::new();
    let mut paths = HashSet::new();
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    for (index, line) in bytes.split(|&byte| byte == b'\n').enumerate() {
        cancel.check()?;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.starts_with(b"#") || line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if entries.len() == MAX_ENTRIES {
            return Err(ChecksumError::TooLarge(manifest.clone()).into());
        }
        let row = index + 1;
        let entry = parse_line(line).map_err(|source| ChecksumError::InvalidLine {
            manifest: manifest.clone(),
            row,
            source,
        })?;
        if !paths.insert(entry.path.clone()) {
            return Err(ChecksumError::DuplicatePath {
                manifest: manifest.clone(),
                row,
                path: entry.path,
            }
            .into());
        }
        entries.push(entry);
    }
    if entries.is_empty() {
        return Err(ChecksumError::EmptyManifest(manifest.clone()).into());
    }
    Ok(entries)
}

pub(super) fn append(
    bytes: &mut Vec<u8>,
    manifest: &Location,
    source: &Location,
    path: &Path,
    digest: [u8; 32],
) -> Result<(), TransferError> {
    let name = path.as_os_str().as_encoded_bytes();
    if relative_path(name).as_deref() != Some(path) {
        return Err(ChecksumError::UnsupportedFilename(source.clone()).into());
    }
    let escaped = name
        .iter()
        .any(|byte| matches!(byte, b'\\' | b'\r' | b'\n'));
    if escaped {
        bytes.push(b'\\');
    }
    for byte in digest {
        write!(bytes, "{byte:02x}")?;
    }
    bytes.extend_from_slice(b"  ");
    for &byte in name {
        match byte {
            b'\\' => bytes.extend_from_slice(b"\\\\"),
            b'\r' => bytes.extend_from_slice(b"\\r"),
            b'\n' => bytes.extend_from_slice(b"\\n"),
            _ => bytes.push(byte),
        }
    }
    bytes.push(b'\n');
    if bytes.len() > MAX_BYTES {
        return Err(ChecksumError::TooLarge(manifest.clone()).into());
    }
    Ok(())
}

fn parse_line(line: &[u8]) -> Result<Entry, ManifestLineError> {
    use ManifestLineError::{Digest, Escape, Format, Path};

    let escaped = line.starts_with(b"\\");
    let line = if escaped { &line[1..] } else { line };
    let (hex, name) = if let Some(tagged) = line.strip_prefix(b"SHA256 (") {
        let split = tagged.len().checked_sub(68).ok_or(Format)?;
        let (name, hex) = tagged.split_at(split);
        (hex.strip_prefix(b") = ").ok_or(Format)?, name)
    } else {
        let (hex, tail) = line.split_at_checked(64).ok_or(Digest)?;
        let name = tail
            .strip_prefix(b"  ")
            .or_else(|| tail.strip_prefix(b" *"))
            .ok_or(Format)?;
        (hex, name)
    };
    let hex: &[u8; 64] = hex.try_into().map_err(|_| Digest)?;
    let mut digest = [0; 32];
    for (out, [high, low]) in digest.iter_mut().zip(hex.as_chunks::<2>().0) {
        let high = char::from(*high).to_digit(16).ok_or(Digest)?;
        let low = char::from(*low).to_digit(16).ok_or(Digest)?;
        *out = (high * 16 + low) as u8;
    }
    let path = if escaped {
        relative_path(&unescape(name).ok_or(Escape)?).ok_or(Path)?
    } else {
        relative_path(name).ok_or(Path)?
    };
    Ok(Entry { path, digest })
}

fn unescape(name: &[u8]) -> Option<Vec<u8>> {
    let mut result = Vec::with_capacity(name.len());
    let mut iter = name.iter().copied();
    while let Some(byte) = iter.next() {
        result.push(match byte {
            b'\\' => match iter.next()? {
                b'\\' => b'\\',
                b'r' => b'\r',
                b'n' => b'\n',
                _ => return None,
            },
            _ => byte,
        });
    }
    Some(result)
}

fn relative_path(bytes: &[u8]) -> Option<PathBuf> {
    if bytes.is_empty()
        || bytes.len() > MAX_PATH_BYTES
        || bytes.ends_with(b"/")
        || bytes.contains(&0)
    {
        return None;
    }
    #[cfg(unix)]
    let name = {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(bytes)
    };
    #[cfg(not(unix))]
    let name = std::ffi::OsStr::new(std::str::from_utf8(bytes).ok()?);
    let mut result = PathBuf::new();
    for (depth, part) in Path::new(name).components().enumerate() {
        if depth >= MAX_DEPTH {
            return None;
        }
        match part {
            Component::CurDir => {}
            Component::Normal(name) => result.push(name),
            _ => return None,
        }
    }
    (!result.as_os_str().is_empty()).then_some(result)
}
