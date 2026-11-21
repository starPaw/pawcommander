//! Mounted filesystem statistics and non-forced macOS ejection.
use crate::i18n::text;
use std::{
    ffi::{c_char, CStr, CString, OsStr},
    io,
    mem::{self, MaybeUninit},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

#[repr(C)]
struct DeviceInfo {
    media_id: u64,
    whole_id: u64,
    placement: i32,
    can_eject: bool,
    name: [c_char; 1024],
    protocol: [c_char; 128],
}

unsafe extern "C" {
    fn paw_volume_info(device: *const c_char, output: *mut DeviceInfo);
    fn paw_volume_matches(
        mount: *const c_char,
        device: *const c_char,
        first: i32,
        second: i32,
        media_id: u64,
        whole_id: u64,
    ) -> bool;
    fn paw_volume_image_attached(id: u64) -> bool;
}

#[derive(Clone)]
pub(crate) struct EjectTarget {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    device: CString,
    fsid: [i32; 2],
    media_id: u64,
    whole_id: u64,
    disk_image: bool,
}

pub(crate) struct Volume {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) filesystem: String,
    pub(crate) protocol: String,
    pub(crate) placement: Placement,
    pub(crate) capacity: Option<u64>,
    pub(crate) available: Option<u64>,
    pub(crate) eject: Option<EjectTarget>,
}

pub(crate) enum Placement {
    Network,
    Internal,
    External,
    DiskImage,
    Unknown,
}

impl Placement {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Network => text!("Сетевой том"),
            Self::Internal => text!("Внутренний диск"),
            Self::External => text!("Внешний диск"),
            Self::DiskImage => text!("Образ диска"),
            Self::Unknown => text!("Подключение неизвестно"),
        }
    }
}

fn mounted_filesystems() -> io::Result<Vec<libc::statfs>> {
    // SAFETY: A null buffer asks Darwin for the mount count.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    let count = usize::try_from(count).map_err(|_| io::Error::last_os_error())?;
    let mut capacity = count.saturating_add(8);
    for _ in 0..3 {
        let bytes = capacity
            .checked_mul(mem::size_of::<libc::statfs>())
            .and_then(|bytes| i32::try_from(bytes).ok())
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EOVERFLOW))?;
        let mut mounts = vec![MaybeUninit::<libc::statfs>::uninit(); capacity];
        // SAFETY: mounts has the expected alignment and bytes of writable storage.
        let written =
            unsafe { libc::getfsstat(mounts.as_mut_ptr().cast(), bytes, libc::MNT_NOWAIT) };
        let written = usize::try_from(written).map_err(|_| io::Error::last_os_error())?;
        if written < capacity {
            return Ok(mounts
                .into_iter()
                .take(written)
                // SAFETY: getfsstat initialized the first written records.
                .map(|mount| unsafe { mount.assume_init() })
                .collect());
        }
        capacity = written.saturating_add(8);
    }
    Err(io::Error::from_raw_os_error(libc::EAGAIN))
}

fn field(value: &[c_char]) -> &CStr {
    // SAFETY: c_char and u8 are byte-sized and every byte value is valid.
    let bytes = unsafe { std::slice::from_raw_parts(value.as_ptr().cast(), value.len()) };
    CStr::from_bytes_until_nul(bytes).unwrap_or(c"")
}

fn mount_path(stat: &libc::statfs) -> &Path {
    Path::new(OsStr::from_bytes(field(&stat.f_mntonname).to_bytes()))
}

fn mount_id(stat: &libc::statfs) -> [i32; 2] {
    // Darwin fsid_t has two int32_t fields. libc exposes the fields privately.
    // SAFETY: The SDK's fsid_t and [i32; 2] have the same layout.
    unsafe { mem::transmute(stat.f_fsid) }
}

fn volume(stat: &libc::statfs) -> Option<Volume> {
    let path = mount_path(stat);
    let root = path == Path::new("/");
    let filesystem = field(&stat.f_fstypename).to_string_lossy();
    let device = field(&stat.f_mntfromname);
    let local_device =
        stat.f_flags & libc::MNT_LOCAL as u32 != 0 && device.to_bytes().starts_with(b"/dev/disk");
    let network = matches!(filesystem.as_ref(), "smbfs" | "nfs" | "webdav" | "afpfs");
    let hidden = stat.f_flags & libc::MNT_DONTBROWSE as u32 != 0;
    if !root
        && (path.starts_with("/System/Volumes")
            || (hidden && filesystem != "smbfs")
            || !(local_device || network || path.starts_with("/Volumes")))
    {
        return None;
    }

    // SAFETY: All fields accept a zero value. The native call writes only this struct.
    let mut info: DeviceInfo = unsafe { mem::zeroed() };
    if local_device {
        // SAFETY: device is terminated and both buffers remain valid through the call.
        unsafe { paw_volume_info(device.as_ptr(), &mut info) };
    }
    let name = field(&info.name).to_string_lossy();
    let name = match (name.is_empty(), path.file_name()) {
        (false, _) => name.into_owned(),
        (true, Some(folder)) => folder.to_string_lossy().into_owned(),
        (true, None) => path.display().to_string(),
    };
    let placement = match (network, info.placement) {
        (true, _) => Placement::Network,
        (_, 1) => Placement::Internal,
        (_, 2) => Placement::External,
        (_, 3) => Placement::DiskImage,
        _ => Placement::Unknown,
    };
    let eject = (!root && info.can_eject).then(|| EjectTarget {
        name: name.clone(),
        path: path.to_owned(),
        device: device.to_owned(),
        fsid: mount_id(stat),
        media_id: info.media_id,
        whole_id: info.whole_id,
        disk_image: matches!(placement, Placement::DiskImage),
    });
    Some(Volume {
        name,
        path: path.to_owned(),
        filesystem: filesystem.into_owned(),
        protocol: field(&info.protocol).to_string_lossy().into_owned(),
        placement,
        capacity: stat.f_blocks.checked_mul(u64::from(stat.f_bsize)),
        available: stat.f_bavail.checked_mul(u64::from(stat.f_bsize)),
        eject,
    })
}

fn volumes(mounts: &[libc::statfs]) -> Vec<Volume> {
    let mut rows: Vec<_> = mounts.iter().filter_map(volume).collect();
    rows.sort_by(|left, right| left.path.cmp(&right.path));
    rows
}

pub(crate) fn scan() -> io::Result<Vec<Volume>> {
    Ok(volumes(&mounted_filesystems()?))
}

pub(crate) struct EjectReport {
    pub(crate) rows: io::Result<Vec<Volume>>,
    pub(crate) detached: Vec<PathBuf>,
    pub(crate) result: Result<(), String>,
}

pub(crate) fn eject(target: &EjectTarget) -> EjectReport {
    let before = mounted_filesystems();
    let result = eject_device(target);
    let after = mounted_filesystems();
    let mut detached = Vec::new();
    if let (Ok(before), Ok(after)) = (&before, &after) {
        detached.extend(
            before
                .iter()
                .filter(|mount| !after.iter().any(|current| same_filesystem(mount, current)))
                .map(|mount| mount_path(mount).to_owned()),
        );
    }
    // Also handle a device removed externally while its confirmation was open.
    let result = result.and_then(|()| match &after {
        Ok(mounts) if mounts.iter().any(|mount| target.matches(mount)) => {
            Err(text!("Том по-прежнему подключён.").into())
        }
        Ok(_) => Ok(()),
        Err(error) => Err(crate::tr_format!(
            "Could not verify ejection ({error}).",
            error = error
        )),
    });
    if after
        .as_ref()
        .is_ok_and(|mounts| !mounts.iter().any(|mount| target.matches(mount)))
    {
        detached.push(target.path.clone());
    }
    EjectReport {
        rows: after.map(|mounts| volumes(&mounts)),
        detached,
        result,
    }
}

fn same_filesystem(left: &libc::statfs, right: &libc::statfs) -> bool {
    mount_id(left) == mount_id(right)
        && mount_path(left) == mount_path(right)
        && field(&left.f_mntfromname) == field(&right.f_mntfromname)
}

impl EjectTarget {
    fn matches(&self, mount: &libc::statfs) -> bool {
        self.fsid == mount_id(mount)
            && self.path == mount_path(mount)
            && self.device.as_c_str() == field(&mount.f_mntfromname)
    }
}

fn eject_device(target: &EjectTarget) -> Result<(), String> {
    let mount = CString::new(target.path.as_os_str().as_bytes())
        .map_err(|_| text!("Выбранный том больше недоступен."))?;
    let mut command = Command::new("/usr/sbin/diskutil");
    command
        .arg("eject")
        .arg(OsStr::from_bytes(target.device.to_bytes()));
    // SAFETY: The buffers are NUL-terminated and remain valid through the call.
    let matches = unsafe {
        paw_volume_matches(
            mount.as_ptr(),
            target.device.as_ptr(),
            target.fsid[0],
            target.fsid[1],
            target.media_id,
            target.whole_id,
        )
    };
    if !matches {
        return Err(text!("Выбранный том больше недоступен.").into());
    }
    // diskutil follows APFS volumes to the actual backing device and safely
    // releases its volumes. Ejecting only the synthesized container can leave
    // its backing disk image attached even when Disk Arbitration reports success.
    let output = command.output().map_err(|error| {
        crate::tr_format!("Could not start macOS ejection ({error}).", error = error)
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map_or_else(
                || output.status.to_string(),
                |line| line.escape_debug().to_string(),
            );
        return Err(crate::tr_format!(
            "macOS could not eject the disk ({reason}). Close any files in use and try again.",
            reason = reason
        ));
    }
    if target.disk_image {
        for _ in 0..20 {
            // SAFETY: The native call accepts a scalar registry identity.
            if !unsafe { paw_volume_image_attached(target.whole_id) } {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
        return Err(text!("Том отключился, но извлечение не подтверждено.").into());
    }
    Ok(())
}
