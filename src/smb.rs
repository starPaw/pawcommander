//! Native SMB shares use a unique, protected mount point and an OS credential entry.
use crate::i18n::text;
use serde::{Deserialize, Serialize};
use std::ffi::{c_char, c_int, CStr, CString};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{
    ffi::OsStrExt,
    fs::{OpenOptionsExt, PermissionsExt},
};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

const SERVICE: &str = "dev.pawcommander.smb.v1";
const RECORD: &str = "connection.json";
const MOUNT_DIR: &str = "mount";
pub(crate) const REPLACE_UNAVAILABLE: crate::i18n::MessageKey =
    crate::i18n::key!("Безопасная замена файла на SMB недоступна. Выберите свободное имя.");

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Endpoint {
    pub(crate) address: String,
    pub(crate) user: Option<String>,
    share: String,
}

impl Endpoint {
    pub(crate) fn parse(address: &str, user: &str, guest: bool) -> Result<Self, String> {
        let invalid =
            || text!("Введите SMB-адрес вида smb://сервер/папка без учётных данных.").to_owned();
        let mut url = url::Url::parse(address.trim()).map_err(|_| invalid())?;
        if url.scheme() != "smb"
            || url.host_str().is_none_or(str::is_empty)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.port() == Some(0)
        {
            return Err(invalid());
        }
        let host = percent_encoding::percent_decode_str(url.host_str().ok_or_else(invalid)?)
            .decode_utf8()
            .map_err(|_| invalid())?;
        let host = url::Host::parse(&host).map_err(|_| invalid())?.to_string();
        url.set_host(Some(&host)).map_err(|_| invalid())?;
        let path = url.path().strip_suffix('/').unwrap_or(url.path());
        let share =
            percent_encoding::percent_decode_str(path.strip_prefix('/').ok_or_else(invalid)?)
                .decode_utf8()
                .map_err(|_| invalid())?
                .into_owned();
        if share.is_empty()
            || matches!(share.as_str(), "." | "..")
            || share.contains(['/', '\\'])
            || share.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        url.path_segments_mut()
            .map_err(|_| invalid())?
            .clear()
            .push(&share);
        if url.port() == Some(445) {
            url.set_port(None).map_err(|_| invalid())?;
        }
        let user = user.trim();
        if !guest && (user.is_empty() || user.chars().any(char::is_control)) {
            return Err(text!("Введите имя пользователя SMB.").into());
        }
        Ok(Self {
            address: url.into(),
            user: (!guest).then(|| user.to_owned()),
            share,
        })
    }

    fn credential(&self) -> Result<keyring::Entry, String> {
        let account = serde_json::to_string(self).map_err(|_| smb_keychain_error())?;
        crate::credentials::system_entry(SERVICE, &account).map_err(|_| smb_keychain_error())
    }

    fn mountpoint(&self, base: &Path) -> PathBuf {
        base.join(MOUNT_DIR).join(&self.share)
    }
}

fn smb_keychain_error() -> String {
    text!("Не удалось открыть или обновить пароль SMB в Связке ключей.").into()
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    endpoint: Endpoint,
    id: [i32; 2],
}

#[derive(Clone)]
pub(crate) struct MountedShare {
    pub(crate) path: PathBuf,
    base: PathBuf,
    record: Record,
}

impl MountedShare {
    pub(crate) fn endpoint(&self) -> &Endpoint {
        &self.record.endpoint
    }

    pub(crate) fn disconnect(&self) -> Result<Option<String>, String> {
        match mount_id(&self.path, true).map_err(mount_error)? {
            Some(id) if id == self.record.id => unmount(&self.path).map_err(|error| {
                crate::tr_format!(
                    "Could not disconnect the SMB share ({error})",
                    error = error
                )
            })?,
            Some(_) => {
                return Err(
                    text!("Подключение SMB изменилось. Обновите список подключений.").into(),
                )
            }
            None => {}
        }
        Ok(remove_mountpoint(&self.base, &self.path)
            .err()
            .map(|error| {
                crate::tr_format!(
                    "SMB disconnected. Could not remove its empty mount folder ({error})",
                    error = error
                )
            }))
    }
}

pub(crate) struct Connected {
    pub(crate) share: MountedShare,
    pub(crate) warning: Option<String>,
}

fn root() -> io::Result<PathBuf> {
    dirs::data_local_dir()
        .map(|root| root.join("PawCommander").join("smb"))
        .ok_or_else(|| io::Error::other(text!("Нет папки данных для подключения SMB.")))
}

pub(crate) fn connect(
    endpoint: Endpoint,
    password: Zeroizing<String>,
    remember: bool,
) -> Result<Connected, String> {
    connect_in(&root().map_err(mount_error)?, endpoint, password, remember)
}

fn connect_in(
    root: &Path,
    endpoint: Endpoint,
    mut password: Zeroizing<String>,
    remember: bool,
) -> Result<Connected, String> {
    if let Some(share) = discover_in(root)
        .into_iter()
        .find(|share| share.endpoint() == &endpoint)
    {
        if password.is_empty() && !remember {
            fs::read_dir(&share.path).map_err(mount_error)?;
            return Ok(Connected {
                share,
                warning: None,
            });
        }
        return Err(text!("Папка уже подключена. Отключите её перед сменой пароля.").into());
    }
    if endpoint.user.is_some() && password.is_empty() {
        match endpoint.credential()?.get_secret() {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                let value = std::str::from_utf8(&bytes).map_err(|_| smb_keychain_error())?;
                password = Zeroizing::new(value.to_owned());
            }
            Err(keyring::Error::NoEntry) => {}
            Err(_) => return Err(smb_keychain_error()),
        }
    }
    if password.contains('\0') {
        return Err(text!("Пароль SMB содержит нулевой байт.").into());
    }
    fs::create_dir_all(root).map_err(mount_error)?;
    let root = root.canonicalize().map_err(mount_error)?;
    // Use only non-recursive cleanup for a directory that can cover remote data.
    let base = tempfile::Builder::new()
        .prefix("connection-")
        .tempdir_in(root)
        .map_err(mount_error)?
        .keep();
    let path = endpoint.mountpoint(&base);
    let result = mount_new(&base, &path, endpoint, &password, remember);
    if result.is_err() {
        if mount_id(&path, true).is_ok_and(|id| id.is_some()) {
            let _ = unmount(&path);
        }
        let _ = remove_mountpoint(&base, &path);
    }
    result
}

fn mount_new(
    base: &Path,
    path: &Path,
    endpoint: Endpoint,
    password: &str,
    remember: bool,
) -> Result<Connected, String> {
    fs::create_dir_all(path).map_err(mount_error)?;
    let covered = File::open(path).map_err(mount_error)?;
    mount(&endpoint, path, password)?;
    let id = mount_id(path, true)
        .map_err(mount_error)?
        .ok_or_else(|| text!("macOS не вернула ожидаемую точку монтирования SMB.").to_owned())?;
    // This handle still refers to the empty local directory underneath the mount.
    covered
        .set_permissions(Permissions::from_mode(0o100))
        .map_err(mount_error)?;
    fs::read_dir(path).map_err(mount_error)?;
    let record = Record { endpoint, id };
    let bytes = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
    fs::write(base.join(RECORD), bytes).map_err(mount_error)?;
    let warning = if remember && record.endpoint.user.is_some() {
        record
            .endpoint
            .credential()
            .and_then(|entry| {
                entry
                    .set_secret(password.as_bytes())
                    .map_err(|_| smb_keychain_error())
            })
            .err()
    } else {
        None
    };
    Ok(Connected {
        share: MountedShare {
            path: path.into(),
            base: base.into(),
            record,
        },
        warning,
    })
}

pub(crate) fn discover() -> Vec<MountedShare> {
    root().map_or_else(|_| Vec::new(), |root| discover_in(&root))
}

fn discover_in(root: &Path) -> Vec<MountedShare> {
    let Ok(root) = root.canonicalize() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            if !entry.file_name().as_bytes().starts_with(b"connection-")
                || !entry.file_type().ok()?.is_dir()
            {
                return None;
            }
            let dir = entry.path();
            let mut data = Vec::new();
            File::open(dir.join(RECORD))
                .ok()?
                .take(16 * 1024)
                .read_to_end(&mut data)
                .ok()?;
            let record: Record = serde_json::from_slice(&data).ok()?;
            let endpoint = Endpoint::parse(
                &record.endpoint.address,
                record.endpoint.user.as_deref().unwrap_or(""),
                record.endpoint.user.is_none(),
            )
            .ok()?;
            let path = endpoint.mountpoint(&dir);
            if endpoint != record.endpoint || mount_id(&path, true).ok().flatten()? != record.id {
                return None;
            }
            Some(MountedShare {
                path,
                base: dir,
                record,
            })
        })
        .collect()
}

fn remove_mountpoint(base: &Path, path: &Path) -> io::Result<()> {
    fn absent_ok(result: io::Result<()>) -> io::Result<()> {
        match result {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
    absent_ok(fs::remove_dir(path))?;
    absent_ok(fs::remove_dir(base.join(MOUNT_DIR)))?;
    absent_ok(fs::remove_file(base.join(RECORD)))?;
    absent_ok(fs::remove_dir(base))?;
    Ok(())
}

fn mount_error(error: io::Error) -> String {
    crate::tr_format!("Could not open the SMB share ({error})", error = error)
}

fn mount(endpoint: &Endpoint, path: &Path, password: &str) -> Result<(), String> {
    let address = CString::new(endpoint.address.as_str()).map_err(|e| e.to_string())?;
    let path = path_cstring(path).map_err(mount_error)?;
    let user = endpoint
        .user
        .as_deref()
        .map(CString::new)
        .transpose()
        .map_err(|e| e.to_string())?;
    // NetFS borrows these buffers only during this call. Credentials are separate from the URL.
    let status = unsafe {
        paw_smb_mount(
            address.as_ptr(),
            path.as_ptr(),
            user.as_ref().map_or(std::ptr::null(), |user| user.as_ptr()),
            password.as_ptr(),
            password.len(),
        )
    };
    match status {
        0 => Ok(()),
        status if status > 0 => Err(mount_error(io::Error::from_raw_os_error(status))),
        status => Err(crate::tr_format!(
            "Could not connect to SMB (NetFS {status})",
            status = status
        )),
    }
}

fn path_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn mount_id(path: &Path, exact: bool) -> io::Result<Option<[i32; 2]>> {
    let path = path_cstring(path)?;
    let mut id = [0; 2];
    let status = unsafe { paw_smb_mount_id(path.as_ptr(), exact, id.as_mut_ptr()) };
    match status {
        0 => Ok(Some(id)),
        libc::ENOENT => Ok(None),
        code => Err(io::Error::from_raw_os_error(code)),
    }
}

pub(crate) fn is_smb_path(path: &Path) -> bool {
    mount_id(path, false).is_ok_and(|id| id.is_some())
}

pub(crate) fn open_upload(path: &Path) -> io::Result<File> {
    let invalid = || io::Error::from(io::ErrorKind::InvalidInput);
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path.parent().ok_or_else(invalid)?)?;
    let parent_fd = parent.as_raw_fd();
    let mut volume = MaybeUninit::<libc::statfs>::uninit();
    // Check the opened directory and create relative to that handle so an
    // unmount or parent rename cannot redirect the upload to local storage.
    if unsafe { libc::fstatfs(parent_fd, volume.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let volume = unsafe { volume.assume_init() };
    if unsafe { CStr::from_ptr(volume.f_fstypename.as_ptr()) }.to_bytes() != b"smbfs" {
        return Err(io::Error::from(io::ErrorKind::Unsupported));
    }
    let name =
        CString::new(path.file_name().ok_or_else(invalid)?.as_bytes()).map_err(|_| invalid())?;
    let fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            0o600 as c_int,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub(crate) fn sync_upload(file: &File) -> io::Result<()> {
    // File::sync_all on macOS asks for F_FULLFSYNC. SMB uses the server's
    // ordinary flush operation, which the native fsync issues.
    if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn unmount(path: &Path) -> io::Result<()> {
    let path = path_cstring(path)?;
    if unsafe { libc::unmount(path.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

unsafe extern "C" {
    fn paw_smb_mount(
        address: *const c_char,
        path: *const c_char,
        user: *const c_char,
        password: *const u8,
        password_len: usize,
    ) -> c_int;
    fn paw_smb_mount_id(path: *const c_char, exact: bool, id: *mut i32) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_normalizes_share_and_keeps_credentials_out_of_the_address() {
        let endpoint =
            Endpoint::parse(" smb://NAS:445/Project%20Files/ ", " WORK\\alice ", false).unwrap();
        assert_eq!(endpoint.address, "smb://nas/Project%20Files");
        assert_eq!(endpoint.share, "Project Files");
        assert_eq!(endpoint.user.as_deref(), Some("WORK\\alice"));
        let guest = Endpoint::parse("smb://[::1]:1445/connection.json", "ignored", true).unwrap();
        assert_eq!(guest.address, "smb://[::1]:1445/connection.json");
        assert_eq!(guest.user, None);
        assert_ne!(
            guest.mountpoint(Path::new("/base")),
            Path::new("/base/connection.json")
        );
    }

    #[test]
    fn endpoint_rejects_credentials_and_paths_outside_one_share() {
        for address in [
            "https://nas/Files",
            "smb://alice:secret@nas/Files",
            "smb://nas/",
            "smb://nas/Files//",
            "smb://nas/Files/a",
            "smb://nas/Files%2Fa",
            "smb://nas/Files%5Ca",
            "smb://nas/%00",
            "smb://bad%20host/Files",
            "smb://nas/Files?query",
            "smb://nas/Files#fragment",
        ] {
            assert!(
                Endpoint::parse(address, "alice", false).is_err(),
                "{address}"
            );
        }
        assert!(Endpoint::parse("smb://nas/Files", "", false).is_err());
    }

    #[test]
    fn upload_and_cleanup_preserve_local_contents() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("connection-test");
        let endpoint = Endpoint::parse("smb://nas/Files", "", true).unwrap();
        let path = endpoint.mountpoint(&base);
        fs::create_dir_all(&path).unwrap();
        let created = path.join("upload");
        assert_eq!(
            open_upload(&created).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert!(!created.exists());
        let existing = path.join("keep");
        fs::write(&existing, b"keep").unwrap();
        assert!(remove_mountpoint(&base, &path).is_err());
        assert_eq!(fs::read(&existing).unwrap(), b"keep");
        fs::remove_file(existing).unwrap();
        fs::write(base.join(RECORD), b"record").unwrap();
        remove_mountpoint(&base, &path).unwrap();
        assert!(!base.exists());
    }
}
