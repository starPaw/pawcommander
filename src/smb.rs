//! Native SMB shares use a unique, protected mount point and an OS credential entry.
use crate::file_error::{AccessAction, AccessError, CredentialFailure, PublicationError};
use crate::operations::TransferError;
use crate::vfs::Location;
use serde::{Deserialize, Serialize};
use std::ffi::{c_char, c_int, CStr, CString};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::{
    ffi::OsStrExt,
    fs::{OpenOptionsExt, PermissionsExt},
};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

mod discovery;
mod error;
mod paths;
pub(crate) use discovery::{Discovery, DiscoveryWarnings};
pub(crate) use error::{MountFailure, PasswordAction, PathAction, RecordProblem, SmbError};
pub(crate) use paths::check_location;

const SERVICE: &str = "dev.pawcommander.smb.v1";
const RECORD: &str = "connection.json";
const MOUNT_DIR: &str = "mount";
const CONNECTION_PREFIX: &str = "connection-";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Endpoint {
    pub(crate) address: String,
    pub(crate) user: Option<String>,
    share: String,
}

impl Endpoint {
    pub(crate) fn parse(address: &str, user: &str, guest: bool) -> Result<Self, SmbError> {
        let mut url = url::Url::parse(address.trim()).map_err(SmbError::AddressSyntax)?;
        if url.scheme() != "smb"
            || url.host_str().is_none_or(str::is_empty)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.port() == Some(0)
        {
            return Err(SmbError::InvalidAddress);
        }
        let host =
            percent_encoding::percent_decode_str(url.host_str().ok_or(SmbError::InvalidAddress)?)
                .decode_utf8()
                .map_err(SmbError::AddressEncoding)?;
        let host = url::Host::parse(&host)
            .map_err(SmbError::AddressSyntax)?
            .to_string();
        url.set_host(Some(&host)).map_err(SmbError::AddressSyntax)?;
        let path = url.path().strip_suffix('/').unwrap_or(url.path());
        let share = percent_encoding::percent_decode_str(
            path.strip_prefix('/').ok_or(SmbError::InvalidAddress)?,
        )
        .decode_utf8()
        .map_err(SmbError::AddressEncoding)?
        .into_owned();
        if share.is_empty()
            || matches!(share.as_str(), "." | "..")
            || share.contains(['/', '\\'])
            || share.chars().any(char::is_control)
        {
            return Err(SmbError::InvalidAddress);
        }
        url.path_segments_mut()
            .map_err(|_| SmbError::InvalidAddress)?
            .clear()
            .push(&share);
        if url.port() == Some(445) {
            url.set_port(None).map_err(|_| SmbError::InvalidAddress)?;
        }
        let user = user.trim();
        if !guest && (user.is_empty() || user.chars().any(char::is_control)) {
            return Err(SmbError::InvalidUser);
        }
        Ok(Self {
            address: url.into(),
            user: (!guest).then(|| user.to_owned()),
            share,
        })
    }

    fn credential(&self, action: PasswordAction) -> Result<keyring::Entry, SmbError> {
        let account = serde_json::to_string(self)
            .map_err(|source| self.password_error(action, CredentialFailure::Profile(source)))?;
        crate::credentials::system_entry(SERVICE, &account)
            .map_err(|source| self.password_error(action, source.into()))
    }

    fn password_error(&self, action: PasswordAction, cause: CredentialFailure) -> SmbError {
        SmbError::Password {
            action,
            address: self.address.clone(),
            cause,
        }
    }

    fn mountpoint(&self, base: &Path) -> PathBuf {
        base.join(MOUNT_DIR).join(&self.share)
    }
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

    fn has_expected_mount(
        &self,
        inspect: impl FnOnce(&Path) -> io::Result<Option<[i32; 2]>>,
    ) -> Result<bool, SmbError> {
        match inspect(&self.path).map_err(PathAction::InspectMount.at(&self.path))? {
            Some(id) if id == self.record.id => Ok(true),
            Some(_) => Err(SmbError::MountChanged(self.path.clone())),
            None => Ok(false),
        }
    }

    fn require_expected_mount(
        &self,
        inspect: impl FnOnce(&Path) -> io::Result<Option<[i32; 2]>>,
    ) -> Result<(), SmbError> {
        if self.has_expected_mount(inspect)? {
            Ok(())
        } else {
            Err(SmbError::MissingMount(self.path.clone()))
        }
    }

    pub(crate) fn check_open(&self) -> Result<(), SmbError> {
        self.require_expected_mount(|path| mount_id(path, true))
            .and_then(|()| {
                fs::read_dir(&self.path)
                    .map(|_| ())
                    .map_err(PathAction::ReadShare.at(&self.path))
            })
            .map_err(|cause| self.open_error(cause))
    }

    fn open_error(&self, cause: SmbError) -> SmbError {
        SmbError::OpenShare {
            address: self.endpoint().address.clone(),
            cause: Box::new(cause),
        }
    }

    pub(crate) fn disconnect(&self) -> Result<Option<SmbError>, SmbError> {
        if self.has_expected_mount(|path| mount_id(path, true))? {
            unmount(&self.path).map_err(PathAction::Unmount.at(&self.path))?;
        }
        let record = self.base.join(RECORD);
        Ok(remove_mountpoint(&self.base, &self.path, Some(&record))
            .err()
            .map(|cleanup| SmbError::DisconnectCleanup {
                address: self.endpoint().address.clone(),
                cleanup: Box::new(cleanup),
            }))
    }
}

pub(crate) struct Connected {
    pub(crate) share: MountedShare,
    pub(crate) warning: Option<SmbError>,
}

fn root() -> Result<PathBuf, SmbError> {
    dirs::data_local_dir()
        .map(|root| root.join("PawCommander").join("smb"))
        .ok_or(SmbError::DataDirectoryUnavailable)
}

pub(crate) fn connect(
    endpoint: Endpoint,
    password: Zeroizing<String>,
    remember: bool,
) -> Result<Connected, SmbError> {
    connect_in(&root()?, endpoint, password, remember)
}

fn connect_in(
    root: &Path,
    endpoint: Endpoint,
    mut password: Zeroizing<String>,
    remember: bool,
) -> Result<Connected, SmbError> {
    if let Some(share) = discover_in(root)
        .shares
        .into_iter()
        .find(|share| share.endpoint() == &endpoint)
    {
        if password.is_empty() && !remember {
            share.check_open()?;
            return Ok(Connected {
                share,
                warning: None,
            });
        }
        return Err(SmbError::AlreadyMounted {
            address: endpoint.address,
            path: share.path,
        });
    }
    if endpoint.user.is_some() && password.is_empty() {
        match endpoint.credential(PasswordAction::Read)?.get_secret() {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                let value =
                    std::str::from_utf8(&bytes).map_err(|source| SmbError::PasswordEncoding {
                        address: endpoint.address.clone(),
                        source,
                    })?;
                password = Zeroizing::new(value.to_owned());
            }
            Err(keyring::Error::NoEntry) => {}
            Err(source) => return Err(endpoint.password_error(PasswordAction::Read, source.into())),
        }
    }
    if password.contains('\0') {
        return Err(SmbError::InvalidPassword);
    }
    fs::create_dir_all(root).map_err(PathAction::CreateStorage.at(root))?;
    let root = root
        .canonicalize()
        .map_err(PathAction::ResolveStorage.at(root))?;
    // Use only non-recursive cleanup for a directory that can cover remote data.
    let base = tempfile::Builder::new()
        .prefix(CONNECTION_PREFIX)
        .tempdir_in(&root)
        .map_err(PathAction::AllocateMount.at(&root))?
        .keep();
    let path = endpoint.mountpoint(&base);
    mount_new(&base, &path, endpoint, &password, remember).map_err(|primary| {
        match cleanup_failed_mount(&base, &path) {
            Ok(()) => primary,
            Err(cleanup) => SmbError::SetupCleanup {
                primary: Box::new(primary),
                cleanup: Box::new(cleanup),
            },
        }
    })
}

fn mount_new(
    base: &Path,
    path: &Path,
    endpoint: Endpoint,
    password: &str,
    remember: bool,
) -> Result<Connected, SmbError> {
    fs::create_dir_all(path).map_err(PathAction::PrepareMount.at(path))?;
    let covered = File::open(path).map_err(PathAction::OpenMount.at(path))?;
    mount(&endpoint, path, password)?;
    let id = mount_id(path, true)
        .map_err(PathAction::InspectMount.at(path))?
        .ok_or_else(|| SmbError::MissingMount(path.to_owned()))?;
    // This handle still refers to the empty local directory underneath the mount.
    covered
        .set_permissions(Permissions::from_mode(0o100))
        .map_err(PathAction::ProtectMount.at(path))?;
    fs::read_dir(path).map_err(PathAction::ReadShare.at(path))?;
    let record = Record { endpoint, id };
    save_record(base, &record)?;
    let warning = if remember && record.endpoint.user.is_some() {
        record
            .endpoint
            .credential(PasswordAction::Save)
            .and_then(|entry| {
                entry.set_secret(password.as_bytes()).map_err(|source| {
                    record
                        .endpoint
                        .password_error(PasswordAction::Save, source.into())
                })
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

fn save_record(base: &Path, record: &Record) -> Result<(), SmbError> {
    let path = base.join(RECORD);
    let bytes = serde_json::to_vec(record).map_err(|source| SmbError::EncodeRecord {
        path: path.clone(),
        source,
    })?;
    let mut pending = tempfile::Builder::new()
        .prefix(".connection-")
        .suffix(".pending")
        .tempfile_in(base)
        .map_err(PathAction::PrepareRecord.at(&path))?;
    pending
        .write_all(&bytes)
        .map_err(PathAction::WriteRecord.at(pending.path()))?;
    pending
        .as_file()
        .sync_all()
        .map_err(PathAction::SyncRecord.at(pending.path()))?;
    crate::fs_ops::rename_no_replace(pending.path(), &path)
        .map_err(PathAction::PublishRecord.at(&path))?;
    pending.disable_cleanup(true);
    Ok(())
}

pub(crate) fn discover() -> Discovery {
    match root() {
        Ok(root) => discover_in(&root),
        Err(error) => {
            let mut result = Discovery::default();
            result.warnings.push(error);
            result
        }
    }
}

fn discover_in(root: &Path) -> Discovery {
    discovery::scan(root, |path| mount_id(path, true))
}

fn cleanup_failed_mount(base: &Path, path: &Path) -> Result<(), SmbError> {
    if mount_id(path, true)
        .map_err(PathAction::InspectMount.at(path))?
        .is_some()
    {
        unmount(path).map_err(PathAction::Unmount.at(path))?;
    }
    // An unsuccessful setup has not published a record. An occupied record belongs to another writer.
    remove_mountpoint(base, path, None)
}

fn remove_mountpoint(base: &Path, path: &Path, record: Option<&Path>) -> Result<(), SmbError> {
    fn absent_ok(result: io::Result<()>, path: &Path) -> Result<(), SmbError> {
        match result {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result.map_err(PathAction::Cleanup.at(path)),
        }
    }
    let container = base.join(MOUNT_DIR);
    absent_ok(fs::remove_dir(path), path)?;
    absent_ok(fs::remove_dir(&container), &container)?;
    if let Some(record) = record {
        absent_ok(fs::remove_file(record), record)?;
    }
    absent_ok(fs::remove_dir(base), base)
}

fn mount(endpoint: &Endpoint, path: &Path, password: &str) -> Result<(), SmbError> {
    let address = CString::new(endpoint.address.as_str()).map_err(|_| SmbError::InvalidAddress)?;
    let native_path = path_cstring(path).map_err(PathAction::PrepareMount.at(path))?;
    let user = endpoint
        .user
        .as_deref()
        .map(CString::new)
        .transpose()
        .map_err(|_| SmbError::InvalidUser)?;
    // NetFS borrows these buffers only during this call. Credentials are separate from the URL.
    let status = unsafe {
        paw_smb_mount(
            address.as_ptr(),
            native_path.as_ptr(),
            user.as_ref().map_or(std::ptr::null(), |user| user.as_ptr()),
            password.as_ptr(),
            password.len(),
        )
    };
    mount_status(endpoint, path, status)
}

fn mount_status(endpoint: &Endpoint, path: &Path, status: c_int) -> Result<(), SmbError> {
    let cause = match status {
        0 => return Ok(()),
        status if status > 0 => MountFailure::Posix(io::Error::from_raw_os_error(status)),
        status => MountFailure::Status(status),
    };
    Err(SmbError::Mount {
        address: endpoint.address.clone(),
        path: path.to_owned(),
        cause,
    })
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

pub(crate) fn open_upload(target: &Location) -> Result<File, TransferError> {
    let invalid = || {
        AccessError::new(
            target,
            AccessAction::CreateFile,
            io::Error::from(io::ErrorKind::InvalidInput),
        )
    };
    let path = target.local().ok_or_else(invalid)?;
    let parent_path = path.parent().ok_or_else(invalid)?;
    let directory = Location::Local(parent_path.to_owned());
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent_path)
        .map_err(|error| AccessError::new(&directory, AccessAction::OpenFile, error))?;
    let parent_fd = parent.as_raw_fd();
    let mut volume = MaybeUninit::<libc::statfs>::uninit();
    let check_volume = |error| AccessError::new(&directory, AccessAction::CheckSmbVolume, error);
    // Check the opened directory and create relative to that handle so an
    // unmount or parent rename cannot redirect the upload to local storage.
    if unsafe { libc::fstatfs(parent_fd, volume.as_mut_ptr()) } != 0 {
        return Err(check_volume(io::Error::last_os_error()).into());
    }
    let volume = unsafe { volume.assume_init() };
    if unsafe { CStr::from_ptr(volume.f_fstypename.as_ptr()) }.to_bytes() != b"smbfs" {
        return Err(check_volume(io::Error::from(io::ErrorKind::Unsupported)).into());
    }
    let name = CString::new(path.file_name().ok_or_else(invalid)?.as_bytes()).map_err(|error| {
        AccessError::new(
            target,
            AccessAction::CreateFile,
            io::Error::new(io::ErrorKind::InvalidInput, error),
        )
    })?;
    let fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            0o600 as c_int,
        )
    };
    if fd < 0 {
        Err(upload_create_error(target, io::Error::last_os_error()))
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn upload_create_error(target: &Location, error: io::Error) -> TransferError {
    let refused = matches!(
        error.kind(),
        io::ErrorKind::AlreadyExists
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::NotFound
            | io::ErrorKind::Unsupported
    );
    let cause = AccessError::new(target, AccessAction::CreateFile, error).into();
    if refused {
        cause
    } else {
        unconfirmed_upload(target, cause)
    }
}

pub(crate) fn unconfirmed_upload(target: &Location, cause: TransferError) -> TransferError {
    TransferError::PublicationUnconfirmed(
        PublicationError::SmbUpload {
            target: target.clone(),
            cause,
        }
        .into(),
    )
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

pub(crate) fn close_upload(file: File) -> io::Result<()> {
    // Ownership moves to close. Retrying a failed close could close a reused fd.
    if unsafe { libc::close(file.into_raw_fd()) } == 0 {
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
        let error =
            Endpoint::parse("smb://user:fixture-only-secret@nas:abc/Files", "", true).unwrap_err();
        assert!(matches!(error, SmbError::AddressSyntax(_)));
        assert!(
            !format!("{error:?}\n{error}\n{}", crate::i18n::smb_error(&error))
                .contains("fixture-only-secret")
        );
    }

    #[test]
    fn mount_diagnostics_include_the_share_path_and_original_status() {
        let endpoint = Endpoint::parse("smb://nas/Files", "", true).unwrap();
        let path = Path::new("/fixture/connection/mount/Files");
        assert!(mount_status(&endpoint, path, 0).is_ok());
        let positive = mount_status(&endpoint, path, libc::EACCES).unwrap_err();
        assert!(
            matches!(&positive, SmbError::Mount { address, path: mount, cause: MountFailure::Posix(source) }
            if address == &endpoint.address && mount == path && source.raw_os_error() == Some(libc::EACCES))
        );
        let negative = mount_status(&endpoint, path, -5999).unwrap_err();
        assert!(matches!(
            &negative,
            SmbError::Mount {
                cause: MountFailure::Status(-5999),
                ..
            }
        ));
        for (error, status) in [(positive, "os error 13"), (negative, "-5999")] {
            let message = crate::i18n::smb_error(&error);
            assert!(message.contains(&endpoint.address));
            assert!(message.contains(path.to_str().unwrap()));
            assert!(message.contains(status));
        }
    }

    #[test]
    fn connection_storage_failure_preserves_the_local_path_and_original_errno() {
        let temp = tempfile::tempdir().unwrap();
        let blocker = temp.path().join("existing file");
        fs::write(&blocker, b"preserve me").unwrap();
        let root = blocker.join("smb");
        let endpoint = Endpoint::parse("smb://nas/Files", "", true).unwrap();
        let Err(error) = connect_in(&root, endpoint, Zeroizing::new(String::new()), false) else {
            panic!("the file must prevent creation of an SMB data folder");
        };
        assert!(
            matches!(&error, SmbError::Local { action: PathAction::CreateStorage, path, source }
            if path == &root && source.raw_os_error() == Some(libc::ENOTDIR))
        );
        assert!(crate::i18n::smb_error(&error).contains(root.to_str().unwrap()));
        assert_eq!(fs::read(blocker).unwrap(), b"preserve me");
    }

    #[test]
    fn published_smb_record_is_private_and_readable_by_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let base = root.join("connection-recorded");
        fs::create_dir(&base).unwrap();
        let record = Record {
            endpoint: Endpoint::parse("smb://NAS/Project%20Files", "alice", false).unwrap(),
            id: [12, 42],
        };
        save_record(&base, &record).unwrap();
        let path = base.join(RECORD);
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(base.read_dir().unwrap().count(), 1);
        let expected_mount = record.endpoint.mountpoint(&base);
        let discovered = discovery::scan(&root, |mount| {
            assert_eq!(mount, expected_mount);
            Ok(Some(record.id))
        });
        assert!(discovered.warnings.entries.is_empty());
        let [share] = discovered.shares.as_slice() else {
            panic!("expected the published record");
        };
        assert_eq!(share.path, expected_mount);
        assert_eq!(share.endpoint(), &record.endpoint);
    }

    #[test]
    fn failed_smb_setup_preserves_occupied_record_files_and_links() {
        use std::os::unix::fs::{symlink, MetadataExt};

        let temp = tempfile::tempdir().unwrap();
        let sentinel = temp.path().join("original");
        fs::write(&sentinel, b"original record bytes").unwrap();
        let record = Record {
            endpoint: Endpoint::parse("smb://nas/Files", "", true).unwrap(),
            id: [12, 42],
        };
        for (name, linked) in [("connection-file", false), ("connection-link", true)] {
            let base = temp.path().join(name);
            let mount = record.endpoint.mountpoint(&base);
            fs::create_dir_all(&mount).unwrap();
            let occupied = base.join(RECORD);
            if linked {
                symlink(&sentinel, &occupied).unwrap();
            } else {
                fs::copy(&sentinel, &occupied).unwrap();
            }
            let inode = occupied.symlink_metadata().unwrap().ino();
            let error = save_record(&base, &record).unwrap_err();
            assert!(
                matches!(&error, SmbError::Local { action: PathAction::PublishRecord, path, source }
                if path == &occupied && source.raw_os_error() == Some(libc::EEXIST))
            );
            let cleanup = cleanup_failed_mount(&base, &mount).unwrap_err();
            assert!(
                matches!(&cleanup, SmbError::Local { action: PathAction::Cleanup, path, source }
                if path == &base && source.kind() == io::ErrorKind::DirectoryNotEmpty)
            );
            assert_eq!(occupied.symlink_metadata().unwrap().ino(), inode);
            assert_eq!(occupied.is_symlink(), linked);
            assert_eq!(fs::read(&occupied).unwrap(), b"original record bytes");
            assert_eq!(base.read_dir().unwrap().count(), 1);
        }
        assert_eq!(fs::read(sentinel).unwrap(), b"original record bytes");
    }

    #[test]
    fn reopening_detached_smb_does_not_accept_its_local_mountpoint() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("connection-detached");
        let endpoint = Endpoint::parse("smb://nas/Files", "", true).unwrap();
        let path = endpoint.mountpoint(&base);
        fs::create_dir_all(&path).unwrap();
        let local = path.join("local-sentinel");
        fs::write(&local, b"unrelated bytes").unwrap();
        let share = MountedShare {
            path: path.clone(),
            base,
            record: Record {
                endpoint,
                id: [12, 42],
            },
        };
        let error = share.check_open().unwrap_err();
        assert!(matches!(&error, SmbError::OpenShare { address, cause }
            if address == "smb://nas/Files" && matches!(cause.as_ref(), SmbError::MissingMount(mount) if mount == &path)));
        let message = crate::i18n::smb_error(&error);
        assert!(message.contains("smb://nas/Files") && message.contains(path.to_str().unwrap()));
        assert_eq!(fs::read(local).unwrap(), b"unrelated bytes");
    }

    #[test]
    fn upload_and_cleanup_preserve_local_contents() {
        use crate::file_error::{AccessCause, Failure};

        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("connection-test");
        let endpoint = Endpoint::parse("smb://nas/Files", "", true).unwrap();
        let path = endpoint.mountpoint(&base);
        fs::create_dir_all(&path).unwrap();
        let created = path.join("upload");
        let error = open_upload(&Location::Local(created.clone())).unwrap_err();
        let TransferError::Failed(Failure::Access(error)) = error else {
            panic!("expected a definite SMB volume check failure");
        };
        assert_eq!(error.location, Location::Local(path.clone()));
        assert_eq!(error.action, AccessAction::CheckSmbVolume);
        assert!(matches!(error.cause, AccessCause::Local(source)
            if source.kind() == io::ErrorKind::Unsupported));
        assert!(!created.exists());
        let existing = path.join("keep");
        fs::write(&existing, b"keep").unwrap();
        let share = MountedShare {
            path: path.clone(),
            base: base.clone(),
            record: Record {
                endpoint,
                id: [0, 0],
            },
        };
        let Some(SmbError::DisconnectCleanup { address, cleanup }) = share.disconnect().unwrap()
        else {
            panic!("a nonempty local mount point must produce a cleanup warning");
        };
        assert_eq!(address, "smb://nas/Files");
        assert!(
            matches!(*cleanup, SmbError::Local { action: PathAction::Cleanup, path: retained, source }
            if retained == path && source.kind() == io::ErrorKind::DirectoryNotEmpty)
        );
        assert_eq!(fs::read(&existing).unwrap(), b"keep");
        fs::remove_file(existing).unwrap();
        fs::write(base.join(RECORD), b"record").unwrap();
        assert!(share.disconnect().unwrap().is_none());
        assert!(!base.exists());
    }

    #[test]
    fn smb_create_rejections_and_uncertainty_retain_the_original_errno() {
        use crate::file_error::{AccessCause, Failure};

        let target = Location::Local("/fixture/SMB/new-file".into());
        for errno in [
            libc::EEXIST,
            libc::EACCES,
            libc::EINVAL,
            libc::ENOENT,
            libc::ENOTSUP,
        ] {
            let result = upload_create_error(&target, io::Error::from_raw_os_error(errno));
            let TransferError::Failed(Failure::Access(error)) = result else {
                panic!("expected a definite rejection");
            };
            assert_eq!(error.location, target);
            assert_eq!(error.action, AccessAction::CreateFile);
            assert!(matches!(error.cause, AccessCause::Local(source)
                if source.raw_os_error() == Some(errno)));
        }
        let error = upload_create_error(&target, io::Error::from_raw_os_error(libc::EIO));
        let TransferError::PublicationUnconfirmed(Failure::Publication(error)) = error else {
            panic!("expected an uncertain create result");
        };
        let PublicationError::SmbUpload {
            target: path,
            cause,
        } = *error
        else {
            panic!("expected account of the SMB upload");
        };
        assert_eq!(path, target);
        let TransferError::Failed(Failure::Access(error)) = cause else {
            panic!("expected a typed create cause");
        };
        assert_eq!(error.location, target);
        assert_eq!(error.action, AccessAction::CreateFile);
        assert!(matches!(error.cause, AccessCause::Local(source)
            if source.raw_os_error() == Some(libc::EIO)));
    }

    #[test]
    fn smb_stop_after_creation_keeps_cancellation_as_the_unconfirmed_cause() {
        use crate::file_error::Failure;

        let target = Location::Local("/fixture/SMB/partial-file".into());
        let error = unconfirmed_upload(&target, TransferError::Cancelled);
        assert!(matches!(
            error,
            TransferError::PublicationUnconfirmed(Failure::Publication(reason))
                if matches!(reason.as_ref(), PublicationError::SmbUpload {
                    target: path, cause: TransferError::Cancelled,
                } if path == &target)
        ));
    }
}
