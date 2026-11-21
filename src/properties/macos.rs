use crate::i18n::text;
use objc2::{rc::autoreleasepool, runtime::AnyObject};
use objc2_foundation::{
    NSArray, NSData, NSPropertyListReadOptions, NSPropertyListSerialization, NSString,
};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs::{self, Metadata};
use std::io;
use std::mem::{size_of, MaybeUninit};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_ATTRIBUTE_BYTES: usize = 64 * 1024;
const FINDER_TAGS: &CStr = c"com.apple.metadata:_kMDItemUserTags";
const ACL_TYPE_EXTENDED: c_int = 0x100;

unsafe extern "C" {
    fn acl_get_link_np(path: *const c_char, kind: c_int) -> *mut c_void;
    fn acl_to_text(acl: *mut c_void, length: *mut libc::ssize_t) -> *mut c_char;
    fn acl_free(value: *mut c_void) -> c_int;
}

pub(super) fn describe(path: &Path, metadata: &Metadata, cancel: &AtomicBool) -> Option<String> {
    let active = || !cancel.load(Ordering::Relaxed);
    if !active() {
        return None;
    }
    let path_bytes = CString::new(path.as_os_str().as_bytes()).ok()?;
    // statfs follows the last component. For a link, report the volume of
    // its directory. The individual metadata calls below all read the link itself.
    let volume_path = if metadata.is_symlink() {
        CString::new(path.parent()?.as_os_str().as_bytes()).ok()?
    } else {
        path_bytes.clone()
    };
    let volume = Volume::read(&volume_path);
    if !active() {
        return None;
    }
    let acl = match volume.acl {
        Some(false) => Err(io::ErrorKind::Unsupported.into()),
        _ => acl_text(&path_bytes),
    };
    if !active() {
        return None;
    }
    let attributes = attribute_names(&path_bytes);
    if !active() {
        return None;
    }
    let tags = finder_tags(&path_bytes);
    let link = metadata
        .is_symlink()
        .then(|| crate::fs_ops::links::describe_target(path));
    if !active() {
        return None;
    }
    // If a root changed while these calls ran, discard the whole extended
    // snapshot. This also disambiguates ENOENT for a missing ACL and a vanished file.
    if !fs::symlink_metadata(path).is_ok_and(|current| same_item(metadata, &current)) {
        return Some(text!("Объект изменился во время чтения метаданных.").into());
    }

    let links = if !metadata.is_file() {
        Ok(Some(text!("Не применяется").into()))
    } else if volume.hard_links == Some(false) {
        Err(io::ErrorKind::Unsupported.into())
    } else {
        Ok(Some(metadata.nlink().to_string()))
    };
    let mut description = format!(
        "{}: {}\n{}: {}\n{}: {}\n{}: {}\n{}: {}",
        text!("Файловая система"),
        volume
            .filesystem
            .as_deref()
            .unwrap_or_else(|| text!("Недоступно")),
        text!("Жёсткие ссылки"),
        display(links),
        text!("Теги Finder"),
        display(tags),
        text!("Расширенные атрибуты"),
        display(attributes),
        "ACL",
        display(acl),
    );
    if let Some(link) = link {
        description.push('\n');
        description.push_str(&link);
    }
    Some(description)
}

fn same_item(before: &Metadata, after: &Metadata) -> bool {
    (
        before.dev(),
        before.ino(),
        before.ctime(),
        before.ctime_nsec(),
    ) == (after.dev(), after.ino(), after.ctime(), after.ctime_nsec())
}

fn display(value: io::Result<Option<String>>) -> String {
    match value {
        Ok(Some(value)) => value,
        Ok(None) => text!("Отсутствуют").into(),
        Err(error)
            if error.kind() == io::ErrorKind::Unsupported
                || matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EOPNOTSUPP)) =>
        {
            text!("Не поддерживается на этом томе").into()
        }
        Err(error) => crate::tr_format!("Unavailable ({error})", error = error),
    }
}

#[derive(Default)]
struct Volume {
    filesystem: Option<String>,
    hard_links: Option<bool>,
    acl: Option<bool>,
}

impl Volume {
    fn read(path: &CStr) -> Self {
        let mut info = MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: path is NUL-terminated and info provides writable storage.
        if unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
            return Self::default();
        }
        // SAFETY: statfs initialized the struct, including the two C strings.
        let info = unsafe { info.assume_init() };
        let filesystem = unsafe { CStr::from_ptr(info.f_fstypename.as_ptr()) };
        let root = unsafe { CStr::from_ptr(info.f_mntonname.as_ptr()) };
        let mut volume = Self {
            filesystem: Some(filesystem.to_string_lossy().into_owned()),
            ..Self::default()
        };
        if let Some(capabilities) = volume_capabilities(root) {
            let supports = |index: usize, flag| {
                (capabilities.valid[index] & flag != 0)
                    .then_some(capabilities.capabilities[index] & flag != 0)
            };
            volume.hard_links =
                supports(libc::VOL_CAPABILITIES_FORMAT, libc::VOL_CAP_FMT_HARDLINKS);
            volume.acl = supports(
                libc::VOL_CAPABILITIES_INTERFACES,
                libc::VOL_CAP_INT_EXTENDED_SECURITY,
            );
        }
        volume
    }
}

fn volume_capabilities(root: &CStr) -> Option<libc::vol_capabilities_attr_t> {
    #[repr(C)]
    struct ResultBuffer {
        length: u32,
        values: libc::vol_capabilities_attr_t,
    }
    // SAFETY: attrlist contains only integer fields and a zeroed reserved field.
    let mut query: libc::attrlist = unsafe { std::mem::zeroed() };
    query.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    query.volattr = libc::ATTR_VOL_INFO | libc::ATTR_VOL_CAPABILITIES;
    let mut buffer = MaybeUninit::<ResultBuffer>::zeroed();
    // SAFETY: the SDK's fixed-size volume capabilities record follows its length word.
    let status = unsafe {
        libc::getattrlist(
            root.as_ptr(),
            (&mut query as *mut libc::attrlist).cast(),
            buffer.as_mut_ptr().cast(),
            size_of::<ResultBuffer>(),
            libc::FSOPT_NOFOLLOW,
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: the buffer is zero-initialized and contains only integers.
    let buffer = unsafe { buffer.assume_init() };
    (buffer.length as usize == size_of::<ResultBuffer>()).then_some(buffer.values)
}

struct AclAllocation(NonNull<c_void>);

impl Drop for AclAllocation {
    fn drop(&mut self) {
        // SAFETY: this is a live allocation returned by the Darwin ACL API.
        unsafe { acl_free(self.0.as_ptr()) };
    }
}

fn acl_text(path: &CStr) -> io::Result<Option<String>> {
    // SAFETY: path is NUL-terminated and the type is Darwin's extended ACL type.
    let Some(acl) = NonNull::new(unsafe { acl_get_link_np(path.as_ptr(), ACL_TYPE_EXTENDED) })
    else {
        let error = io::Error::last_os_error();
        // Darwin also uses ENOENT for an existing object with no extended ACL.
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(None)
        } else {
            Err(error)
        };
    };
    let acl = AclAllocation(acl);
    // SAFETY: acl is live and acl_to_text permits a null length out-parameter.
    let formatted = unsafe { acl_to_text(acl.0.as_ptr(), ptr::null_mut()) };
    let formatted =
        AclAllocation(NonNull::new(formatted.cast()).ok_or_else(io::Error::last_os_error)?);
    // SAFETY: acl_to_text returns an allocated, NUL-terminated string.
    let formatted = unsafe { CStr::from_ptr(formatted.0.as_ptr().cast()) }.to_string_lossy();
    let entries = formatted
        .lines()
        .filter(|line| !line.starts_with("!#acl "))
        .collect::<Vec<_>>()
        .join("\n");
    Ok((!entries.is_empty()).then_some(entries))
}

fn bounded_attribute(read: impl FnOnce(&mut [u8]) -> libc::ssize_t) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; MAX_ATTRIBUTE_BYTES];
    let count = read(&mut bytes);
    if count < 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ERANGE) {
            Err(attribute_limit())
        } else {
            Err(error)
        };
    }
    if count as usize > bytes.len() {
        return Err(attribute_limit());
    }
    bytes.truncate(count as usize);
    Ok(bytes)
}

fn attribute_limit() -> io::Error {
    io::Error::other(text!("Превышен лимит предпросмотра атрибутов"))
}

fn attribute_names(path: &CStr) -> io::Result<Option<String>> {
    let bytes = bounded_attribute(|buffer| {
        // SAFETY: path is NUL-terminated and buffer is writable for its full length.
        unsafe {
            libc::listxattr(
                path.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                libc::XATTR_NOFOLLOW,
            )
        }
    })?;
    let mut names = bytes
        .split(|&byte| byte == 0)
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    names.sort_unstable();
    Ok((!names.is_empty()).then(|| {
        names
            .iter()
            .map(|name| visible_name(&String::from_utf8_lossy(name)))
            .collect::<Vec<_>>()
            .join(", ")
    }))
}

fn finder_tags(path: &CStr) -> io::Result<Option<String>> {
    let bytes = bounded_attribute(|buffer| {
        // SAFETY: both input C strings are NUL-terminated. The output fits in buffer.
        unsafe {
            libc::getxattr(
                path.as_ptr(),
                FINDER_TAGS.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
                libc::XATTR_NOFOLLOW,
            )
        }
    });
    match bytes {
        Ok(bytes) => decode_tags(&bytes),
        Err(error) if error.raw_os_error() == Some(libc::ENOATTR) => Ok(None),
        Err(error) => Err(error),
    }
}

fn decode_tags(bytes: &[u8]) -> io::Result<Option<String>> {
    let malformed = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            text!("Не удалось прочитать формат тегов Finder"),
        )
    };
    autoreleasepool(|_| {
        let data = NSData::with_bytes(bytes);
        // SAFETY: a null format out-parameter is permitted by Foundation.
        let plist = unsafe {
            NSPropertyListSerialization::propertyListWithData_options_format_error(
                &data,
                NSPropertyListReadOptions::Immutable,
                ptr::null_mut(),
            )
        }
        .map_err(|_| malformed())?;
        let tags = plist
            .downcast::<NSArray<AnyObject>>()
            .map_err(|_| malformed())?;
        let mut names = String::new();
        for value in tags.iter() {
            let tag = value
                .downcast::<NSString>()
                .map_err(|_| malformed())?
                .to_string();
            // Finder uses a final newline and a single color digit after the name.
            let name = match tag.rsplit_once('\n') {
                Some((name, suffix)) if matches!(suffix.as_bytes(), [b'0'..=b'7']) => name,
                _ => &tag,
            };
            if names.len() + name.len() + 2 > MAX_ATTRIBUTE_BYTES {
                return Err(attribute_limit());
            }
            if !names.is_empty() {
                names.push_str(", ");
            }
            names.push_str(&visible_name(name));
        }
        Ok((!names.is_empty()).then_some(names))
    })
}

fn visible_name(name: &str) -> String {
    name.replace(char::is_control, " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    fn path_c(path: &Path) -> CString {
        CString::new(path.as_os_str().as_bytes()).unwrap()
    }

    const TAGS_PLIST: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <plist version=\"1.0\"><array><string>Синий\n4</string><string>Plain</string></array></plist>";

    #[test]
    fn reads_metadata_for_the_selected_inode_without_following_a_symbolic_link() {
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("original");
        let hard = folder.path().join("hard-link");
        let symbolic = folder.path().join("symbolic");
        fs::write(&file, b"unchanged").unwrap();
        fs::hard_link(&file, &hard).unwrap();
        symlink(&file, &symbolic).unwrap();
        let path = path_c(&hard);
        assert!(finder_tags(&path).unwrap().is_none());

        // SAFETY: all pointers refer to live buffers for the duration of setxattr.
        assert_eq!(
            unsafe {
                libc::setxattr(
                    path.as_ptr(),
                    FINDER_TAGS.as_ptr(),
                    TAGS_PLIST.as_ptr().cast(),
                    TAGS_PLIST.len(),
                    0,
                    libc::XATTR_NOFOLLOW,
                )
            },
            0,
            "{}",
            io::Error::last_os_error()
        );
        assert!(Command::new("/bin/chmod")
            .args(["+a", "everyone allow read"])
            .arg(&hard)
            .status()
            .unwrap()
            .success());

        let snapshot = fs::symlink_metadata(&hard).unwrap();
        assert_eq!(snapshot.nlink(), 2);
        let path = path_c(&file);
        assert_eq!(finder_tags(&path).unwrap().as_deref(), Some("Синий, Plain"));
        assert!(attribute_names(&path)
            .unwrap()
            .unwrap()
            .contains("_kMDItemUserTags"));
        assert!(acl_text(&path).unwrap().unwrap().contains("allow:read"));
        let symbolic_path = path_c(&symbolic);
        assert!(finder_tags(&symbolic_path).unwrap().is_none());
        assert!(!attribute_names(&symbolic_path)
            .unwrap()
            .is_some_and(|names| names.contains("_kMDItemUserTags")));
        assert!(acl_text(&symbolic_path).unwrap().is_none());

        fs::remove_file(&hard).unwrap();
        fs::write(&hard, b"replacement").unwrap();
        let changed = describe(&hard, &snapshot, &AtomicBool::new(false));
        assert_eq!(
            changed.as_deref(),
            Some(text!("Объект изменился во время чтения метаданных."))
        );
        assert_eq!(fs::read(&file).unwrap(), b"unchanged");
    }

    #[test]
    fn malformed_finder_tags_are_a_read_error() {
        let invalid = br#"<?xml version="1.0"?><plist version="1.0"><array><string>tag</string><integer>7</integer></array></plist>"#;
        assert_eq!(
            decode_tags(invalid).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
