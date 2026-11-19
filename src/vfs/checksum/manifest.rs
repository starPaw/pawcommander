use crate::operations::{Cancellation, TransferError};
use std::{
    collections::HashSet,
    io::Write,
    path::{Component, Path, PathBuf},
};

pub(super) const MAX_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_ENTRIES: usize = 50_000;
const MAX_PATH_BYTES: usize = 4096;
const MAX_DEPTH: usize = 128;

pub(super) struct Entry {
    pub(super) path: PathBuf,
    pub(super) digest: [u8; 32],
}

pub(super) fn parse(bytes: &[u8], cancel: &Cancellation) -> Result<Vec<Entry>, TransferError> {
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
            return Err(too_large());
        }
        let entry = parse_line(line).ok_or_else(|| {
            let row = index + 1;
            crate::tr_format!(
                "Некорректная строка манифеста SHA-256 №{row}",
                "Invalid SHA-256 manifest line {row}"
            )
        })?;
        if !paths.insert(entry.path.clone()) {
            let row = index + 1;
            return Err(crate::tr_format!(
                "Повторяющийся путь в строке манифеста №{row}",
                "Duplicate path in manifest line {row}"
            )
            .into());
        }
        entries.push(entry);
    }
    if entries.is_empty() {
        return Err(crate::i18n::text("Манифест SHA-256 пуст").into());
    }
    Ok(entries)
}

pub(super) fn append(
    bytes: &mut Vec<u8>,
    path: &Path,
    digest: [u8; 32],
) -> Result<(), TransferError> {
    let name = path.as_os_str().as_encoded_bytes();
    if relative_path(name).as_deref() != Some(path) {
        return Err(crate::tr_format!(
            "Имя файла не подходит для манифеста: {}",
            "Filename cannot be used in the manifest: {}",
            path.display()
        )
        .into());
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
        return Err(too_large());
    }
    Ok(())
}

pub(super) fn too_large() -> TransferError {
    crate::i18n::text("Манифест SHA-256 превышает 16 МиБ или 50 000 записей").into()
}

fn parse_line(line: &[u8]) -> Option<Entry> {
    let escaped = line.starts_with(b"\\");
    let line = if escaped { &line[1..] } else { line };
    let (hex, name) = if let Some(tagged) = line.strip_prefix(b"SHA256 (") {
        let (name, hex) = tagged.split_at_checked(tagged.len().checked_sub(68)?)?;
        (hex.strip_prefix(b") = ")?, name)
    } else {
        let (hex, tail) = line.split_at_checked(64)?;
        let name = tail
            .strip_prefix(b"  ")
            .or_else(|| tail.strip_prefix(b" *"))?;
        (hex, name)
    };
    let hex: &[u8; 64] = hex.try_into().ok()?;
    let mut digest = [0; 32];
    for (out, [high, low]) in digest.iter_mut().zip(hex.as_chunks::<2>().0) {
        let high = char::from(*high).to_digit(16)?;
        let low = char::from(*low).to_digit(16)?;
        *out = (high * 16 + low) as u8;
    }
    let path = if escaped {
        relative_path(&unescape(name)?)?
    } else {
        relative_path(name)?
    };
    Some(Entry { path, digest })
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
