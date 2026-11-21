use crate::{app::SavedPath, i18n::text, operations::TransferError};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, CString, OsStr},
    fs, io,
    mem::{size_of, MaybeUninit},
    os::{macos::fs::MetadataExt, unix::ffi::OsStrExt},
    path::{Path, PathBuf},
};

unsafe extern "C" {
    // Public Darwin SDK interface in <sys/fsgetpath.h>.
    fn fsgetpath(
        buffer: *mut libc::c_char,
        size: usize,
        filesystem: *mut libc::fsid_t,
        file_id: u64,
    ) -> libc::ssize_t;
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Reference {
    pub(super) path: SavedPath,
    identity: Identity,
}

#[derive(Clone, Serialize, Deserialize)]
struct Identity {
    mount: SavedPath,
    volume: Option<[u8; 16]>,
    device: u64,
    inode: u64,
    birth: (i64, i64),
    folder: bool,
}

struct Volume {
    mount: PathBuf,
    fsid: libc::fsid_t,
    uuid: Option<[u8; 16]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Unavailable {
    Missing,
    InTrash,
    Read(String),
}

impl Unavailable {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Missing => text!("Объект отсутствует или том недоступен"),
            Self::InTrash => text!("Объект в Корзине"),
            Self::Read(error) => error,
        }
    }
}

impl Reference {
    pub(super) fn capture(source: &Path) -> Result<Self, TransferError> {
        let before = fs::symlink_metadata(source)?;
        if !before.is_file() && !before.is_dir() {
            return Err(text!("Выберите обычные файлы или папки").into());
        }
        let path = fs::canonicalize(source)?;
        let volume = Volume::of(&path)?;
        if in_trash(&path, &volume.mount) {
            return Err(text!("Объект в Корзине").into());
        }
        let identity = Identity::capture(&before, &volume);
        if !identity.matches_metadata(&fs::symlink_metadata(&path)?)
            || !identity.matches_metadata(&fs::symlink_metadata(source)?)
        {
            return Err(text!("Файл или папка изменились").into());
        }
        Ok(Self {
            path: path.as_path().into(),
            identity,
        })
    }

    pub(super) fn same_item(&self, other: &Self) -> bool {
        self.identity.same_item(&other.identity)
    }

    pub(super) fn is_folder(&self) -> bool {
        self.identity.folder
    }

    pub(super) fn resolve(&mut self) -> Result<PathBuf, Unavailable> {
        let saved = self.path.clone().into_path().ok_or(Unavailable::Missing)?;
        let previous = match self.check(&saved) {
            Ok(identity) => {
                self.identity = identity;
                return Ok(saved);
            }
            Err(Unavailable::InTrash) => return Err(Unavailable::InTrash),
            Err(error) => error,
        };
        let Some(volume) = self.identity.mounted_volume() else {
            return Err(previous);
        };
        let Some(path) = volume.path_for(self.identity.inode) else {
            return Err(previous);
        };
        let path = fs::canonicalize(path).map_err(unavailable)?;
        self.identity = self.check(&path)?;
        self.path = path.as_path().into();
        Ok(path)
    }

    fn check(&self, path: &Path) -> Result<Identity, Unavailable> {
        if !path.is_absolute() {
            return Err(Unavailable::Missing);
        }
        let meta = fs::symlink_metadata(path).map_err(unavailable)?;
        if !self.identity.matches_kind(&meta) {
            return Err(Unavailable::Missing);
        }
        let volume = Volume::of(path).map_err(unavailable)?;
        let identity = Identity::capture(&meta, &volume);
        if !self.identity.same_item(&identity)
            || fs::canonicalize(path).map_err(unavailable)? != path
        {
            return Err(Unavailable::Missing);
        }
        if in_trash(path, &volume.mount) {
            return Err(Unavailable::InTrash);
        }
        Ok(identity)
    }
}

impl Identity {
    fn capture(meta: &fs::Metadata, volume: &Volume) -> Self {
        Self {
            mount: volume.mount.as_path().into(),
            volume: volume.uuid,
            device: meta.st_dev(),
            inode: meta.st_ino(),
            birth: (meta.st_birthtime(), meta.st_birthtime_nsec()),
            folder: meta.is_dir(),
        }
    }

    fn same_item(&self, other: &Self) -> bool {
        let same_volume = match (self.volume, other.volume) {
            (Some(left), Some(right)) => left == right,
            (None, None) => self.device == other.device,
            _ => false,
        };
        same_volume
            && self.inode == other.inode
            && self.birth == other.birth
            && self.folder == other.folder
    }

    fn matches_metadata(&self, meta: &fs::Metadata) -> bool {
        self.matches_kind(meta)
            && self.device == meta.st_dev()
            && self.inode == meta.st_ino()
            && self.birth == (meta.st_birthtime(), meta.st_birthtime_nsec())
    }

    fn matches_kind(&self, meta: &fs::Metadata) -> bool {
        if self.folder {
            meta.is_dir()
        } else {
            meta.is_file()
        }
    }

    fn mounted_volume(&self) -> Option<Volume> {
        if let Some(mount) = self.mount.clone().into_path()
            && let Ok(volume) = Volume::of(&mount)
            && volume.mount == mount
            && self.volume == volume.uuid
            && (self.volume.is_some() || mount.metadata().ok()?.st_dev() == self.device)
        {
            return Some(volume);
        }
        Volume::by_uuid(self.volume?)
    }
}

impl Volume {
    fn of(path: &Path) -> io::Result<Self> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        let mut stat = MaybeUninit::uninit();
        // SAFETY: The path is NUL-terminated and stat has the output layout.
        if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: statfs initialized stat on success.
        Ok(Self::from_stat(unsafe { stat.assume_init() }))
    }

    fn from_stat(stat: libc::statfs) -> Self {
        // SAFETY: Darwin supplies a NUL-terminated mount name inside statfs.
        let mount = unsafe { CStr::from_ptr(stat.f_mntonname.as_ptr()) };
        let uuid = volume_uuid(mount);
        let mount = PathBuf::from(OsStr::from_bytes(mount.to_bytes()));
        Self {
            mount,
            fsid: stat.f_fsid,
            uuid,
        }
    }

    fn by_uuid(uuid: [u8; 16]) -> Option<Self> {
        // SAFETY: A null buffer asks Darwin for the count of mounted volumes.
        let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        let count = usize::try_from(count).ok()?;
        let mut mounts = vec![MaybeUninit::<libc::statfs>::uninit(); count.saturating_add(8)];
        let bytes = i32::try_from(mounts.len().checked_mul(size_of::<libc::statfs>())?).ok()?;
        // SAFETY: mounts owns bytes of output storage, correctly aligned for statfs.
        let written =
            unsafe { libc::getfsstat(mounts.as_mut_ptr().cast(), bytes, libc::MNT_NOWAIT) };
        mounts
            .into_iter()
            .take(usize::try_from(written).ok()?)
            // SAFETY: Only the records initialized by a successful call are read.
            .map(|stat| unsafe { stat.assume_init() })
            .filter(|stat| stat.f_flags & libc::MNT_LOCAL as u32 != 0)
            .map(Self::from_stat)
            .find(|volume| volume.uuid == Some(uuid))
    }

    fn path_for(mut self, inode: u64) -> Option<PathBuf> {
        let mut buffer = [0u8; libc::PATH_MAX as usize];
        // SAFETY: Both pointers refer to initialized storage and the length is
        // the buffer's exact capacity. The returned path is bounded below.
        let length = unsafe {
            fsgetpath(
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut self.fsid,
                inode,
            )
        };
        let length = usize::try_from(length).ok()?;
        if length == 0 || length > buffer.len() {
            return None;
        }
        let path = CStr::from_bytes_until_nul(&buffer).ok()?;
        Some(PathBuf::from(OsStr::from_bytes(path.to_bytes())))
    }
}

fn volume_uuid(mount: &CStr) -> Option<[u8; 16]> {
    #[repr(C)]
    struct Response {
        length: u32,
        uuid: [u8; 16],
    }
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        commonattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut response = Response {
        length: 0,
        uuid: [0; 16],
    };
    // SAFETY: The specified volume attributes return a size word and a UUID.
    // Darwin receives a NUL-terminated mount path and a bounded output buffer.
    let result = unsafe {
        libc::getattrlist(
            mount.as_ptr(),
            (&mut attributes as *mut libc::attrlist).cast(),
            (&mut response as *mut Response).cast(),
            size_of::<Response>(),
            0,
        )
    };
    (result == 0 && response.length as usize == size_of::<Response>() && response.uuid != [0; 16])
        .then_some(response.uuid)
}

fn in_trash(path: &Path, mount: &Path) -> bool {
    if path.starts_with(mount.join(".Trashes")) {
        return true;
    }
    path.components().any(|part| part.as_os_str() == ".Trash")
        && dirs::home_dir().is_some_and(|home| {
            path.starts_with(home.join(".Trash"))
                || fs::canonicalize(home).is_ok_and(|home| path.starts_with(home.join(".Trash")))
        })
}

fn unavailable(error: io::Error) -> Unavailable {
    match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => Unavailable::Missing,
        _ => Unavailable::Read(error.to_string()),
    }
}
