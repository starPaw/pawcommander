//! Preview and apply a bounded, Windows-compatible naming plan from macOS.
mod execute;
mod names;
#[cfg(test)]
mod tests;

pub(crate) use names::Reason;

use super::{
    compare::{MAX_ENTRIES, MAX_PATH_BYTES},
    guarded_directory::Directory,
    read_scope::ReadScope,
    transfer::{entry_metadata, mounted_smb, transfer_plan, TransferItem, TransferSourceManifest},
    Location,
};
use crate::{
    fs_ops::TransferKind,
    i18n::text,
    operations::{Cancellation, Progress, TransferError},
};
use names::{destination_bytes, metadata_name, path_limit, portable_key, Name};
use std::{
    collections::{BTreeMap, HashSet},
    ffi::{CStr, CString},
    fs, io,
    mem::MaybeUninit,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

// LocalStaging uses a u32 process ID, u128 nanosecond timestamp and u64 counter.
const STAGED_ROOT: usize =
    "/.pawcommander-transfer-4294967295-340282366920938463463374607431768211455-18446744073709551615/payload"
        .len();

pub(crate) struct Change {
    pub(crate) before: PathBuf,
    pub(crate) after: PathBuf,
    pub(crate) reasons: Vec<Reason>,
}

pub(crate) struct Plan {
    destination: Directory,
    roots: Vec<Root>,
    kind: TransferKind,
    pub(crate) changes: Vec<Change>,
    pub(crate) entries: usize,
    pub(crate) direct: bool,
    apple_double: bool,
}

pub(crate) fn build(
    sources: &[Location],
    destination: &Location,
    kind: TransferKind,
    progress: &mut Progress,
) -> Result<Plan, TransferError> {
    if sources.is_empty()
        || sources
            .iter()
            .chain([destination])
            .any(|path| path.local().is_none())
    {
        return Err(
            text!("Для совместимого переноса выберите объекты в двух локальных папках").into(),
        );
    }
    if sources.iter().chain([destination]).any(mounted_smb) {
        return Err(text!("Совместимый перенос недоступен на томах SMB").into());
    }
    let scope = ReadScope::open(destination)?;
    let destination = Directory::from_scope(&scope)?;
    let limits = Limits::read(destination.location.path())?;
    if limits.read_only {
        return Err(text!("Том назначения доступен только для чтения").into());
    }
    let direct = limits.exfat;
    let staging = destination.location.path().as_os_str().len() + STAGED_ROOT;
    if !direct && staging > limits.path {
        return Err(path_limit());
    }

    let selected = transfer_plan(sources, &destination.location, kind, progress)?;
    let cancel = progress.cancellation();
    let root_bytes = limits.component_at(destination.location.path().as_os_str().len(), None);
    let mut occupied = names_in(&destination, &cancel)?;
    let mut candidates: Vec<_> = selected
        .into_iter()
        .map(|item| {
            cancel.check()?;
            let name = Name::suggest(
                item.target.path().file_name().expect("planned target"),
                root_bytes,
                limits.apple_double,
            )?;
            Ok((item, name))
        })
        .collect::<Result<_, TransferError>>()?;
    candidates.sort_by_key(|(_, name)| !name.reasons.is_empty());
    let mut preview = Plan {
        destination,
        roots: Vec::new(),
        kind,
        changes: Vec::new(),
        entries: 0,
        direct,
        apple_double: limits.apple_double,
    };
    let mut path_bytes = 0;
    for (mut item, suggestion) in candidates {
        progress.checkpoint()?;
        if kind == TransferKind::Move {
            let volume = Limits::read(item.source.path())?;
            if volume.apple_double || volume.read_only {
                return Err(text!(
                    "Для этого исходного тома выберите копирование с совместимыми именами",
                )
                .into());
            }
        }
        let name = suggestion.reserve(root_bytes, limits.apple_double, &mut occupied, |name| {
            cancel.check()?;
            Ok(entry_metadata(&preview.destination.location.join(name))?.is_some())
        })?;
        item.target = preview.destination.location.join(&name.value);
        let manifest = TransferSourceManifest::capture_move_observed(
            &item.source,
            &cancel,
            &mut |path, _| progress.item(path.path()),
        )?;
        let root_metadata = &manifest.entries[&item.source].metadata;
        if manifest
            .entries
            .values()
            .any(|entry| root_metadata.same_local_volume(&entry.metadata) != Some(true))
        {
            return Err(
                text!("Совместимый перенос не пересекает точки монтирования внутри папки").into(),
            );
        }
        preview.entries += manifest.entries.len();
        // The output manifest also counts AppleDouble companions on FAT.
        if preview.entries > MAX_ENTRIES
            || (limits.apple_double && manifest.entries.len() > MAX_ENTRIES / 2)
        {
            return Err(limit());
        }
        let stage = (!direct).then_some(staging);
        let mut layout = Layout {
            manifest: &manifest,
            target: &item.target,
            input_root: item.source.path().parent().expect("selected item"),
            relative_root: Path::new(&name.value),
            stage,
            limits,
            paths: BTreeMap::new(),
            changes: &mut preview.changes,
            path_bytes: &mut path_bytes,
            cancel: &cancel,
        };
        layout.walk(&item.source, PathBuf::new(), name.reasons)?;
        let paths = layout.paths;
        preview.roots.push(Root {
            item,
            manifest,
            paths,
        });
    }
    preview.destination.verify()?;
    cancel.check()?;
    Ok(preview)
}

struct Root {
    item: TransferItem,
    manifest: TransferSourceManifest,
    // Each source path has the reviewed relative path beneath the new root.
    paths: BTreeMap<Location, PathBuf>,
}

struct Layout<'a> {
    manifest: &'a TransferSourceManifest,
    target: &'a Location,
    input_root: &'a Path,
    relative_root: &'a Path,
    stage: Option<usize>,
    limits: Limits,
    paths: BTreeMap<Location, PathBuf>,
    changes: &'a mut Vec<Change>,
    path_bytes: &'a mut usize,
    cancel: &'a Cancellation,
}

impl Layout<'_> {
    fn walk(
        &mut self,
        source: &Location,
        relative: PathBuf,
        reasons: Vec<Reason>,
    ) -> Result<(), TransferError> {
        self.cancel.check()?;
        let before = source
            .path()
            .strip_prefix(self.input_root)
            .expect("source in tree");
        *self.path_bytes += source.path().as_os_str().len() + relative.as_os_str().len();
        if *self.path_bytes > MAX_PATH_BYTES {
            return Err(limit());
        }
        if !reasons.is_empty() {
            let after = if relative.as_os_str().is_empty() {
                self.relative_root.to_owned()
            } else {
                self.relative_root.join(&relative)
            };
            self.changes.push(Change {
                before: before.to_owned(),
                after,
                reasons,
            });
        }
        let entry = &self.manifest.entries[source];
        if !entry.metadata.is_file() && entry.children.is_none() {
            return Err(crate::tr_format!(
                "Compatible transfer supports regular files and folders. Check {source}.",
                source = source
            )
            .into());
        }
        let root = self.target.path().as_os_str().len() - self.relative_root.as_os_str().len()
            + destination_bytes(
                self.relative_root.to_str().expect("planned Unicode name"),
                self.limits.apple_double,
            );
        let tail = destination_bytes(
            relative.to_str().expect("planned Unicode path"),
            self.limits.apple_double,
        ) + usize::from(!relative.as_os_str().is_empty());
        let bytes = self
            .limits
            .component_at(root + tail, self.stage.map(|base| base + tail));
        self.paths.insert(source.clone(), relative.clone());
        let Some(children) = &entry.children else {
            return Ok(());
        };
        let mut candidates: Vec<_> = children
            .iter()
            .map(|name| {
                self.cancel.check()?;
                Ok((name, Name::suggest(name, bytes, self.limits.apple_double)?))
            })
            .collect::<Result<_, TransferError>>()?;
        // Assign compatible originals before a rewritten sibling can claim them.
        candidates.sort_by_key(|(_, name)| !name.reasons.is_empty());
        let mut occupied = HashSet::new();
        for (original, suggestion) in candidates {
            let name =
                suggestion.reserve(bytes, self.limits.apple_double, &mut occupied, |_| {
                    self.cancel.check()?;
                    Ok(false)
                })?;
            self.walk(
                &source.join(original),
                relative.join(&name.value),
                name.reasons,
            )?;
        }
        Ok(())
    }
}

pub(super) fn names_in(
    folder: &Directory,
    cancel: &Cancellation,
) -> Result<HashSet<String>, TransferError> {
    folder.verify()?;
    let mut names = HashSet::new();
    for item in fs::read_dir(folder.location.path())? {
        cancel.check()?;
        names.insert(portable_key(&item?.file_name()));
    }
    folder.verify()?;
    Ok(names)
}

#[derive(Clone, Copy)]
struct Limits {
    component: usize,
    path: usize,
    exfat: bool,
    apple_double: bool,
    read_only: bool,
}

impl Limits {
    fn read(path: &Path) -> Result<Self, TransferError> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::from)?;
        let mut info = MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: path is terminated and the output buffer matches statfs.
        if unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: Darwin initializes the buffer and terminates the file-system name.
        let info = unsafe { info.assume_init() };
        let format = unsafe { CStr::from_ptr(info.f_fstypename.as_ptr()) };
        let calc = |key, fallback| {
            // SAFETY: The same terminated path remains valid for this call.
            let length = unsafe { libc::pathconf(path.as_ptr(), key) };
            usize::try_from(length)
                .ok()
                .filter(|&size| size > 0)
                .unwrap_or(fallback)
        };
        Ok(Self {
            component: calc(libc::_PC_NAME_MAX, 255),
            path: calc(libc::_PC_PATH_MAX, libc::PATH_MAX as usize)
                .min(libc::PATH_MAX as usize)
                .saturating_sub(1),
            exfat: format.to_bytes() == b"exfat",
            apple_double: matches!(format.to_bytes(), b"exfat" | b"msdos"),
            read_only: info.f_flags & libc::MNT_RDONLY as u32 != 0,
        })
    }

    fn component_at(&self, final_parent: usize, stage_parent: Option<usize>) -> usize {
        let parent = final_parent.max(stage_parent.unwrap_or_default());
        let available = self.component.min(self.path.saturating_sub(parent + 1));
        // Leave room for the native ._ companion as well as the data name.
        available.saturating_sub(2 * usize::from(self.apple_double))
    }
}

fn limit() -> TransferError {
    text!("План совместимых имён превышает допустимый размер").into()
}
