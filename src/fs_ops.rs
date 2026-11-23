//! Local filesystem operations. Copies are staged privately on the destination
//! volume before publication. This does not snapshot a changing filesystem.

use crate::file_error::{
    AccessAction, AccessCause, AccessError, CopyVerificationError, Failure, ListingError,
    ListingErrorKind, LocalPathAction, MutationError, PathError, ScanError, SpaceError,
    StagingCleanupError, StagingError, TransferPlanError,
};
use crate::operations::{
    access_io, AccessStream, Cancellation, CopyReader, Progress, TransferError,
};
use crate::vfs::{DeletionFailure, Location, MAX_TREE_DEPTH};
#[cfg(target_os = "macos")]
use crate::{file_error::SmbMutationAction, smb::MutationMount};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
pub(crate) mod links;

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub metadata: ListingMetadata,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ListingMetadata {
    pub mode: Option<u16>,
    pub owner_id: Option<u32>,
    pub created: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    pub(crate) local_file: Option<LocalFileIdentity>,
    #[cfg(target_os = "macos")]
    pub(crate) link_status: Option<links::LinkStatus>,
    #[cfg(target_os = "macos")]
    pub(crate) smb_mount: Option<[i32; 2]>,
}

impl ListingMetadata {
    fn from_local(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        let (mode, owner_id) = {
            use std::os::unix::fs::MetadataExt;
            (
                Some((metadata.mode() & 0o7777) as u16),
                Some(metadata.uid()),
            )
        };
        #[cfg(not(unix))]
        let (mode, owner_id) = (None, None);
        Self {
            mode,
            owner_id,
            created: metadata.created().ok(),
            accessed: metadata.accessed().ok(),
            local_file: LocalFileIdentity::from_metadata(metadata),
            #[cfg(target_os = "macos")]
            link_status: None,
            #[cfg(target_os = "macos")]
            smb_mount: None,
        }
    }
}

/// Additional version information for regular local files, captured by the listing's lstat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct LocalFileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}

impl LocalFileIdentity {
    #[cfg(target_os = "macos")]
    pub(crate) fn device_and_inode(self) -> (u64, u64) {
        (self.device, self.inode)
    }

    pub(crate) fn from_metadata(metadata: &fs::Metadata) -> Option<Self> {
        if !metadata.is_file() {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
            })
        }
        #[cfg(not(unix))]
        Some(Self {
            created: metadata.created().ok(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferKind {
    Copy,
    Move,
}

pub fn read_directory(
    path: &Path,
    cancelled: &impl Fn() -> bool,
) -> Result<Vec<FileEntry>, TransferError> {
    let mut entries = Vec::new();
    visit_directory(path, cancelled, |entry| entries.push(entry))?;
    if !cancelled() {
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
    }
    Ok(entries)
}

pub fn visit_directory(
    path: &Path,
    cancelled: &impl Fn() -> bool,
    visit: impl FnMut(FileEntry),
) -> Result<(), TransferError> {
    if cancelled() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    let location = Location::Local(path.to_owned());
    #[cfg(target_os = "macos")]
    let mount = crate::smb::check_location(&location)?;
    let result = visit_local_directory(
        path,
        cancelled,
        visit,
        #[cfg(target_os = "macos")]
        mount,
    );
    #[cfg(target_os = "macos")]
    if let Some(id) = mount
        && !cancelled()
    {
        crate::smb::verify_listing_mount(&location, id)?;
    }
    result
}

fn visit_local_directory(
    path: &Path,
    cancelled: &impl Fn() -> bool,
    mut visit: impl FnMut(FileEntry),
    #[cfg(target_os = "macos")] mount: Option<[i32; 2]>,
) -> Result<(), TransferError> {
    let mut directory = fs::read_dir(path)
        .map_err(|error| ListingError::new(path.to_owned(), ListingErrorKind::OpenLocal(error)))?;
    while !cancelled() {
        let Some(item) = directory.next() else {
            break;
        };
        if cancelled() {
            break;
        }
        let item = item.map_err(|error| {
            ListingError::new(path.to_owned(), ListingErrorKind::ReadLocalEntry(error))
        })?;
        let path = item.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // Another app may remove a file while the listing is being read.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(ListingError::new(path, ListingErrorKind::LocalMetadata(error)).into());
            }
        };
        if cancelled() {
            break;
        }
        let is_symlink = metadata.file_type().is_symlink();
        let target = is_symlink.then(|| fs::metadata(&path));
        let listing = ListingMetadata::from_local(&metadata);
        #[cfg(target_os = "macos")]
        let listing = ListingMetadata {
            link_status: target.as_ref().map(links::LinkStatus::from_stat),
            smb_mount: mount,
            ..listing
        };
        visit(FileEntry {
            name: item.file_name().to_string_lossy().into_owned(),
            path,
            is_dir: target
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .unwrap_or(&metadata)
                .is_dir(),
            is_symlink,
            size: metadata.len(),
            modified: metadata.modified().ok(),
            metadata: listing,
        });
    }
    Ok(())
}

pub fn create_directory(parent: &Path, name: &str) -> Result<PathBuf, TransferError> {
    validate_name(name)?;
    #[cfg(target_os = "macos")]
    let mut mount = {
        let mut mount = MutationMount::new(SmbMutationAction::CreateFolder);
        mount.check(&parent.join(name))?;
        mount
    };
    let parent = directory_path(parent)?;
    let destination = parent.join(name);
    let error_at =
        |error| MutationError::local(&destination, LocalPathAction::CreateDirectory, error);
    #[cfg(target_os = "macos")]
    {
        mount.check(&destination)?;
        if let Some(parent) = mount.open_parent(&destination)? {
            use rustix::fs::{mkdirat, Mode};

            mkdirat(&parent, name, Mode::from_raw_mode(0o777))
                .map_err(|error| error_at(error.into()))?;
            return Ok(destination);
        }
    }
    fs::create_dir(&destination).map_err(error_at)?;
    Ok(destination)
}

pub fn rename_entry(source: &Path, name: &str) -> Result<PathBuf, TransferError> {
    rename_entry_checked(
        source,
        name,
        #[cfg(target_os = "macos")]
        MutationMount::new(SmbMutationAction::Rename),
    )
}

pub(crate) fn rename_entry_checked(
    source: &Path,
    name: &str,
    #[cfg(target_os = "macos")] mut mount: MutationMount,
) -> Result<PathBuf, TransferError> {
    validate_name(name)?;
    #[cfg(target_os = "macos")]
    let source = entry_path(source, |path| mount.check(path))?;
    #[cfg(not(target_os = "macos"))]
    let source = entry_path(source, |_| Ok(()))?;
    fs::symlink_metadata(&source)
        .map_err(|error| MutationError::local(&source, LocalPathAction::ReadMetadata, error))?;
    let destination = source
        .parent()
        .ok_or_else(|| MutationError::RenameRoot(source.clone().into()))?
        .join(name);
    if source == destination {
        return Ok(source);
    }
    #[cfg(target_os = "macos")]
    mount.check(&destination)?;
    ensure_absent(&destination)?;
    #[cfg(target_os = "macos")]
    if let Some(parent) = mount.open_parent(&source)? {
        rename_between_open_directories(&parent, &parent, &source, &destination)?;
        return Ok(destination);
    }
    rename_with_context(&source, &destination)?;
    Ok(destination)
}

pub(crate) struct LocalTransfer {
    pub original: PathBuf,
    pub source: PathBuf,
    pub target: PathBuf,
    pub covered: Vec<PathBuf>,
    pub source_metadata: fs::Metadata,
}

pub(crate) fn plan_transfer(
    sources: &[PathBuf],
    destination: &Path,
    progress: &mut Progress,
) -> Result<Vec<LocalTransfer>, TransferError> {
    progress.checkpoint()?;
    progress.item(destination);
    let resolved_destination = directory_path(destination)?;
    let sources = normalize_selection(
        sources,
        progress,
        #[cfg(target_os = "macos")]
        None,
    )?;
    let mut names = HashMap::new();
    let mut plan = Vec::<LocalTransfer>::new();
    for LocalSelection {
        original,
        source,
        covered,
        metadata,
        ..
    } in sources
    {
        progress.checkpoint()?;
        progress.item(&original);
        let current = fs::symlink_metadata(&source)
            .map_err(|error| MutationError::local(&source, LocalPathAction::ReadMetadata, error))?;
        if current.is_dir() && resolved_destination.starts_with(&source) {
            return Err(TransferPlanError::IntoSource {
                source: original.into(),
                destination: destination.to_owned().into(),
            }
            .into());
        }
        let name = source
            .file_name()
            .ok_or_else(|| TransferPlanError::MissingName(original.clone().into()))?;
        let target = resolved_destination.join(name);
        if let Some(first) = names.insert(name.to_os_string(), plan.len()) {
            return Err(TransferPlanError::DuplicateTarget {
                first: plan[first].original.clone().into(),
                second: original.into(),
                target: target.into(),
            }
            .into());
        }
        plan.push(LocalTransfer {
            original,
            source,
            target,
            covered,
            source_metadata: metadata,
        });
    }
    Ok(plan)
}

#[cfg(test)]
fn transfer(sources: &[PathBuf], destination: &Path, kind: TransferKind) -> Result<usize, String> {
    let sources: Vec<_> = sources.iter().cloned().map(Into::into).collect();
    crate::vfs::transfer(&sources, &destination.to_path_buf().into(), kind)
}

pub fn trash_entries(paths: &[PathBuf], progress: &mut Progress) -> Result<usize, DeletionFailure> {
    let paths = normalize_selection(
        paths,
        progress,
        #[cfg(target_os = "macos")]
        Some(SmbMutationAction::Trash),
    )?;
    progress.results.plan(
        paths
            .iter()
            .map(|entry| (crate::vfs::Location::Local(entry.original.clone()), None)),
    );
    for (completed, entry) in paths.iter().enumerate() {
        let original = crate::vfs::Location::Local(entry.original.clone());
        progress.results.begin(&original);
        let path = &entry.source;
        progress.item(path);
        let result = (|| {
            #[cfg(target_os = "macos")]
            if let Some(mount) = &entry.mount {
                progress.check_cancelled()?;
                mount.recheck()?;
            }
            let cancel = progress.cancellation();
            let captured = SourceManifest::capture_observed(path, &cancel, &mut |observed, _| {
                if let Ok(relative) = observed.strip_prefix(path) {
                    progress.results.removing(&original.join(relative));
                }
            });
            let captured = match captured.and_then(|manifest| manifest.verify(&cancel, |_, _| {})) {
                Ok(()) => true,
                Err(error) if TransferError::is_cancelled_io(&error) => return Err(error.into()),
                Err(_) => {
                    progress.results.abandon_group();
                    false
                }
            };
            progress.check_cancelled()?;
            #[cfg(target_os = "macos")]
            let _parent = match &entry.mount {
                Some(mount) => mount.open_parent(path)?,
                None => None,
            };
            // Use the operating system's Trash, with no permanent-deletion fallback.
            trash::delete(path).map_err(|source| MutationError::Trash {
                path: path.clone(),
                source,
            })?;
            if captured {
                progress.results.trash_descendants_unconfirmed();
            }
            Ok(())
        })();
        progress.results.removed(&original, &result);
        result.map_err(|error| DeletionFailure {
            error,
            completed,
            partial_remote: false,
        })?;
    }
    Ok(paths.len())
}

pub(crate) fn validate_name(name: &str) -> Result<(), PathError> {
    let mut components = Path::new(name).components();
    if name.trim().is_empty()
        || name.contains(['/', '\\', '\0'])
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(PathError::InvalidLocalName(name.to_owned()));
    }
    Ok(())
}

fn directory_path(path: &Path) -> Result<PathBuf, TransferError> {
    let path = fs::canonicalize(path)
        .map_err(|error| MutationError::local(path, LocalPathAction::OpenDirectory, error))?;
    let metadata = fs::metadata(&path)
        .map_err(|error| MutationError::local(&path, LocalPathAction::OpenDirectory, error))?;
    if !metadata.is_dir() {
        return Err(MutationError::NotDirectory(path).into());
    }
    Ok(path)
}

/// Resolve parent aliases but preserve the final component: canonicalizing a
/// selected symlink itself would accidentally operate on the link's target.
fn entry_path(
    path: &Path,
    mut verify: impl FnMut(&Path) -> Result<(), TransferError>,
) -> Result<PathBuf, TransferError> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|source| MutationError::CurrentDirectory {
                input: path.to_owned(),
                source,
            })?
            .join(path)
    };
    verify(&absolute)?;
    let root = || MutationError::ModifyRoot(absolute.clone());
    let name = absolute.file_name().ok_or_else(root)?;
    let parent = absolute.parent().ok_or_else(root)?;
    let parent = fs::canonicalize(parent)
        .map_err(|error| MutationError::local(parent, LocalPathAction::ResolveParent, error))?;
    let entry = parent.join(name);
    if entry != absolute {
        verify(&entry)?;
    }
    let metadata = fs::symlink_metadata(&entry)
        .map_err(|error| MutationError::local(&entry, LocalPathAction::ReadMetadata, error))?;
    if metadata.file_type().is_symlink() {
        Ok(entry)
    } else {
        // Resolve the actual spelling on case-insensitive volumes as well as
        // parent aliases, so descendant protection compares canonical paths.
        let canonical = fs::canonicalize(&entry)
            .map_err(|error| MutationError::local(&entry, LocalPathAction::ResolveEntry, error))?;
        if canonical != entry {
            verify(&canonical)?;
        }
        Ok(canonical)
    }
}

struct LocalSelection {
    original: PathBuf,
    source: PathBuf,
    covered: Vec<PathBuf>,
    metadata: fs::Metadata,
    #[cfg(target_os = "macos")]
    mount: Option<MutationMount>,
}

fn normalize_selection(
    paths: &[PathBuf],
    progress: &mut Progress,
    #[cfg(target_os = "macos")] action: Option<SmbMutationAction>,
) -> Result<Vec<LocalSelection>, TransferError> {
    let mut entries = Vec::with_capacity(paths.len());
    for original in paths {
        progress.checkpoint()?;
        progress.item(original);
        #[cfg(target_os = "macos")]
        let mut mount = action.map(MutationMount::new);
        #[cfg(target_os = "macos")]
        let source = entry_path(original, |path| match &mut mount {
            Some(mount) => mount.check(path),
            None => Ok(()),
        })?;
        #[cfg(not(target_os = "macos"))]
        let source = entry_path(original, |_| Ok(()))?;
        let metadata = fs::symlink_metadata(&source)
            .map_err(|error| MutationError::local(&source, LocalPathAction::ReadMetadata, error))?;
        entries.push(LocalSelection {
            original: original.clone(),
            source,
            covered: Vec::new(),
            metadata,
            #[cfg(target_os = "macos")]
            mount,
        });
    }
    let mut first = HashMap::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        progress.checkpoint()?;
        first.entry(entry.source.as_path()).or_insert(index);
    }
    let mut retained = vec![false; entries.len()];
    let mut covered = vec![Vec::new(); entries.len()];
    for (index, entry) in entries.iter().enumerate() {
        progress.checkpoint()?;
        let path = &entry.source;
        // The outermost selected directory covers all selected entries in
        // its tree, including paths spelled through other parent aliases.
        let representative = path
            .ancestors()
            .skip(1)
            .filter_map(|ancestor| first.get(ancestor).copied())
            .filter(|&ancestor| entries[ancestor].metadata.is_dir())
            .last()
            .unwrap_or(first[path.as_path()]);
        if index == representative {
            retained[index] = true;
        } else {
            covered[representative].push(entry.original.clone());
        }
    }
    Ok(entries
        .into_iter()
        .enumerate()
        .filter(|(index, _)| retained[*index])
        .map(|(index, mut entry)| {
            entry.covered = std::mem::take(&mut covered[index]);
            entry
        })
        .collect())
}

fn ensure_absent(path: &Path) -> Result<(), TransferError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(MutationError::DestinationExists(path.to_owned().into()).into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(MutationError::local(path, LocalPathAction::CheckDestination, error).into())
        }
    }
}

static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

struct StagingDirectory {
    path: PathBuf,
    #[cfg(unix)]
    original: fs::Metadata,
    #[cfg(windows)]
    original_id: crate::windows_file_id::WindowsFileId,
}

impl StagingDirectory {
    fn new(path: PathBuf) -> Result<Self, TransferError> {
        let location = Location::Local(path.clone());
        let inspect = |error| AccessError::new(&location, AccessAction::ReadMetadata, error);
        let original = fs::symlink_metadata(&path).map_err(inspect)?;
        if !original.is_dir() || original.file_type().is_symlink() {
            return Err(StagingError::IdentityChanged(location).into());
        }
        #[cfg(windows)]
        let original_id = crate::windows_file_id::from_path(&path)
            .map_err(inspect)?
            .ok_or_else(|| StagingError::IdentityUnavailable(location))?;
        Ok(Self {
            path,
            #[cfg(unix)]
            original,
            #[cfg(windows)]
            original_id,
        })
    }

    fn same_owned(&self) -> io::Result<bool> {
        let candidate = fs::symlink_metadata(&self.path)?;
        if !candidate.is_dir() || candidate.file_type().is_symlink() {
            return Ok(false);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(self.original.dev() == candidate.dev() && self.original.ino() == candidate.ino())
        }
        #[cfg(windows)]
        {
            Ok(crate::windows_file_id::from_path(&self.path)? == Some(self.original_id))
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(false)
        }
    }

    fn ensure_owned(&self) -> Result<(), TransferError> {
        let location = Location::Local(self.path.clone());
        if self
            .same_owned()
            .map_err(|error| AccessError::new(&location, AccessAction::ReadMetadata, error))?
        {
            Ok(())
        } else {
            Err(StagingError::IdentityChanged(location).into())
        }
    }
}

fn create_staging_directory(parent: &Path) -> Result<StagingDirectory, TransferError> {
    for _ in 0..100 {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".pawcommander-copy-{}-{unique}-{counter}",
            std::process::id()
        ));
        let location = Location::Local(path.clone());
        crate::recovery::plan_staging(&location)?;
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        match builder.create(&path) {
            Ok(()) => return StagingDirectory::new(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                crate::recovery::abandon_staging(&location)?;
                continue;
            }
            Err(e) => {
                return Err(local_copy_error(
                    Some(parent),
                    access_io(&location, AccessAction::CreateStagingDirectory, e),
                ));
            }
        }
    }
    Err(StagingError::NamesExhausted(Location::Local(parent.to_owned())).into())
}

#[cfg(test)]
fn copy_atomically(source: &Path, destination: &Path) -> Result<(), String> {
    copy_atomically_with_progress(
        source,
        destination,
        &mut Progress::default(),
        |payload, destination| {
            rename_no_replace(payload, destination).map_err(|e| {
                crate::tr_format!(
                    "Could not finish copying “{}” to “{}”: {e}",
                    source.display(),
                    destination.display(),
                    e = e
                )
            })
        },
    )
}

#[cfg(test)]
pub(crate) fn copy_atomically_with_progress(
    source: &Path,
    destination: &Path,
    progress: &mut Progress,
    publish: impl FnOnce(&Path, &Path) -> Result<(), String>,
) -> Result<(), String> {
    copy_atomically_checked(source, destination, progress, publish, &mut |_| Ok(()))
}

#[cfg(test)]
fn copy_atomically_checked(
    source: &Path,
    destination: &Path,
    progress: &mut Progress,
    publish: impl FnOnce(&Path, &Path) -> Result<(), String>,
    after_entry: &mut impl FnMut(&Path) -> io::Result<()>,
) -> Result<(), String> {
    copy_atomically_cancellable(
        source,
        destination,
        progress,
        |payload, target| publish(payload, target).map_err(Into::into),
        after_entry,
    )
    .map_err(|error| crate::i18n::file_error(&error))
}

pub(crate) fn copy_atomically_cancellable(
    source: &Path,
    destination: &Path,
    progress: &mut Progress,
    publish: impl FnOnce(&Path, &Path) -> Result<(), TransferError>,
    after_entry: &mut impl FnMut(&Path) -> io::Result<()>,
) -> Result<(), TransferError> {
    progress.checkpoint()?;
    progress.item(source);
    let cancel = progress.cancellation();
    let source_location = crate::vfs::Location::Local(source.to_path_buf());
    let target_location = crate::vfs::Location::Local(destination.to_path_buf());
    let scan = cancel.clone().pausable_scan();
    let manifest = SourceManifest::capture_observed(source, &scan, &mut |path, error| {
        let location = crate::vfs::Location::Local(path.to_path_buf());
        if let Some(error) = error {
            progress.results.staged_io(&location, Some(error));
        } else {
            progress
                .results
                .capturing(&source_location, &target_location, &location);
        }
    })?;
    progress.results.captured();
    let parent = destination
        .parent()
        .ok_or_else(|| SpaceError::MissingDestinationParent(target_location.clone()))?;
    check_local_copy_space(manifest.bytes, parent)?;
    progress.checkpoint()?;
    let staging = create_staging_directory(parent)?;
    let location = Location::Local(staging.path.clone());
    if let Err(error) = crate::recovery::register_staging(&location) {
        return StagingCleanupError::combine(
            Err(error),
            remove_staging(&staging),
            &location,
            &target_location,
        );
    }
    staging.ensure_owned()?;
    let payload = staging.path.join("payload");
    let result = copy_tree(
        source,
        &payload,
        destination,
        progress,
        &manifest,
        after_entry,
    )
    .and_then(|()| progress.checkpoint())
    // Revisit every entry, including early files after later files copied.
    .and_then(|()| {
        manifest.verify(&cancel, |source, error| {
            progress.results.staged_io(
                &crate::vfs::Location::Local(source.to_path_buf()),
                Some(error),
            );
        })
    })
    .and_then(|()| cancel.check())
    .map_err(|error| local_copy_error(Some(parent), error))
    .and_then(|()| staging.ensure_owned())
    // No cancellation inside publication, rollback, or staging cleanup.
    .and_then(|()| publish(&payload, destination));
    StagingCleanupError::combine(
        result,
        remove_staging(&staging),
        &location,
        &target_location,
    )
}

#[cfg(unix)]
fn available_copy_space(parent: &Path) -> io::Result<u128> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(parent.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    Ok((stat.f_bavail as u128) * (stat.f_frsize as u128))
}

#[cfg(windows)]
fn available_copy_space(parent: &Path) -> io::Result<u128> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            available: *mut u64,
            total: *mut u64,
            free: *mut u64,
        ) -> i32;
    }
    let mut path: Vec<u16> = parent.as_os_str().encode_wide().collect();
    if path.contains(&0) {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    // Also handles a UNC share root, for which Win32 requires a trailing slash.
    if !matches!(path.last(), Some(47 | 92)) {
        path.push(92);
    }
    path.push(0);
    let mut available = 0u64;
    // Available-to-caller respects user quotas; volume-wide free bytes do not.
    if unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(u128::from(available))
}

#[cfg(not(any(unix, windows)))]
fn available_copy_space(_parent: &Path) -> io::Result<u128> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

pub(crate) fn check_local_copy_space(required: u64, parent: &Path) -> Result<(), TransferError> {
    let available = available_copy_space(parent).map_err(|source| SpaceError::LocalQuery {
        directory: parent.to_owned(),
        source,
    })?;
    ensure_copy_space(required, available, &Location::Local(parent.to_owned()))
}

pub(crate) fn local_copy_error(parent: Option<&Path>, error: io::Error) -> TransferError {
    if let Some(parent) = parent
        && matches!(
            error.kind(),
            io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
        )
    {
        if let Some(access) = error
            .get_ref()
            .and_then(|cause| cause.downcast_ref::<AccessError>())
            && !matches!(
                access,
                AccessError {
                    action: AccessAction::CopyEntry
                        | AccessAction::CreateFile
                        | AccessAction::CreateDirectory
                        | AccessAction::CreateStagingDirectory
                        | AccessAction::CreateLink
                        | AccessAction::WriteFile
                        | AccessAction::FlushFile
                        | AccessAction::SyncFile
                        | AccessAction::WriteMetadata
                        | AccessAction::WritePermissions,
                    cause: AccessCause::Local(_),
                    ..
                }
            )
        {
            return error.into();
        }
        SpaceError::LocalWrite {
            directory: parent.to_owned(),
            source: error,
        }
        .into()
    } else {
        error.into()
    }
}

pub(crate) fn local_copy_write_error(
    target: &Location,
    action: AccessAction,
    error: io::Error,
) -> TransferError {
    local_copy_error(
        target.local().and_then(Path::parent),
        access_io(target, action, error),
    )
}

pub(crate) fn ensure_copy_space(
    required: u64,
    available: u128,
    directory: &Location,
) -> Result<(), TransferError> {
    if u128::from(required) > available {
        Err(SpaceError::Insufficient {
            directory: directory.clone(),
            required,
            available,
        }
        .into())
    } else {
        Ok(())
    }
}

struct SourceEntry {
    metadata: fs::Metadata,
    children: Option<Vec<OsString>>,
    link: Option<PathBuf>,
}

struct SourceManifest {
    entries: BTreeMap<PathBuf, SourceEntry>,
    bytes: u64,
}

fn source_changed(path: &Path) -> io::Error {
    io::Error::other(Failure::from(MutationError::SourceChanged(
        path.to_owned().into(),
    )))
}

fn source_io(path: &Path, action: AccessAction, error: io::Error) -> io::Error {
    access_io(&Location::Local(path.to_owned()), action, error)
}

fn source_metadata(path: &Path) -> io::Result<fs::Metadata> {
    fs::symlink_metadata(path).map_err(|error| source_io(path, AccessAction::ReadMetadata, error))
}

fn source_link(path: &Path) -> io::Result<PathBuf> {
    fs::read_link(path).map_err(|error| source_io(path, AccessAction::ReadLink, error))
}

fn unsupported_copy_source(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        Failure::from(ScanError::LocalCopySpecialFile(path.to_owned().into())),
    )
}

fn directory_names(path: &Path, cancel: &Cancellation) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    cancel.check()?;
    let read_error = |error| source_io(path, AccessAction::ReadChildren, error);
    for entry in fs::read_dir(path).map_err(read_error)? {
        cancel.check()?;
        names.push(entry.map_err(read_error)?.file_name());
    }
    cancel.check()?;
    names.sort();
    cancel.check()?;
    Ok(names)
}

// Shared with local sources uploaded through the SFTP transfer path.
pub(crate) fn source_metadata_matches(
    expected: &fs::Metadata,
    actual: &fs::Metadata,
) -> io::Result<bool> {
    let matches = expected.file_type() == actual.file_type()
        && expected.len() == actual.len()
        && expected.modified()? == actual.modified()?
        && expected.created().ok() == actual.created().ok()
        && expected.permissions() == actual.permissions();
    #[cfg(unix)]
    let matches = {
        use std::os::unix::fs::MetadataExt;
        matches
            && expected.dev() == actual.dev()
            && expected.ino() == actual.ino()
            && expected.ctime() == actual.ctime()
            && expected.ctime_nsec() == actual.ctime_nsec()
    };
    Ok(matches)
}

impl SourceEntry {
    fn verify_metadata(&self, path: &Path, actual: &fs::Metadata) -> io::Result<()> {
        let matches = source_metadata_matches(&self.metadata, actual)
            .map_err(|error| source_io(path, AccessAction::ReadMetadata, error))?;
        if matches {
            Ok(())
        } else {
            Err(source_changed(path))
        }
    }

    fn verify(&self, path: &Path, cancel: &Cancellation) -> io::Result<()> {
        cancel.check()?;
        let actual = source_metadata(path)?;
        self.verify_metadata(path, &actual)?;
        if let Some(names) = &self.children
            && *names != directory_names(path, cancel)?
        {
            return Err(source_changed(path));
        }
        if let Some(link) = &self.link
            && *link != source_link(path)?
        {
            return Err(source_changed(path));
        }
        Ok(())
    }
}

impl SourceManifest {
    #[cfg(test)]
    fn capture(path: &Path) -> io::Result<Self> {
        Self::capture_cancellable(path, &Cancellation::default())
    }

    #[cfg(test)]
    fn capture_cancellable(path: &Path, cancel: &Cancellation) -> io::Result<Self> {
        Self::capture_observed(path, cancel, &mut |_, _| {})
    }

    fn capture_observed(
        path: &Path,
        cancel: &Cancellation,
        observe: &mut impl FnMut(&Path, Option<&io::Error>),
    ) -> io::Result<Self> {
        let mut manifest = Self {
            entries: BTreeMap::new(),
            bytes: 0,
        };
        manifest.capture_entry(path, 0, cancel, observe)?;
        Ok(manifest)
    }

    fn capture_entry(
        &mut self,
        path: &Path,
        depth: usize,
        cancel: &Cancellation,
        observe: &mut impl FnMut(&Path, Option<&io::Error>),
    ) -> io::Result<()> {
        observe(path, None);
        let result = self.capture_entry_inner(path, depth, cancel, observe);
        if let Err(error) = &result {
            observe(path, Some(error));
        }
        result
    }

    fn capture_entry_inner(
        &mut self,
        path: &Path,
        depth: usize,
        cancel: &Cancellation,
        observe: &mut impl FnMut(&Path, Option<&io::Error>),
    ) -> io::Result<()> {
        cancel.check()?;
        if depth > MAX_TREE_DEPTH {
            return Err(io::Error::other(Failure::from(
                ScanError::TransferDepthLimit {
                    location: path.to_owned().into(),
                    limit: MAX_TREE_DEPTH,
                },
            )));
        }
        let metadata = source_metadata(path)?;
        // Do not silently claim a metadata check when the filesystem cannot
        // supply the modification time.
        metadata
            .modified()
            .map_err(|error| source_io(path, AccessAction::ReadMetadata, error))?;
        let children = if metadata.is_dir() {
            Some(directory_names(path, cancel)?)
        } else {
            None
        };
        let link = if metadata.is_symlink() {
            Some(source_link(path)?)
        } else {
            None
        };
        if metadata.is_file() {
            self.bytes = self.bytes.checked_add(metadata.len()).ok_or_else(|| {
                io::Error::other(Failure::from(ScanError::TransferSizeOverflow(
                    path.to_owned().into(),
                )))
            })?;
        } else if !metadata.is_dir() && !metadata.is_symlink() {
            return Err(unsupported_copy_source(path));
        }
        if let Some(names) = &children {
            for name in names {
                self.capture_entry(&path.join(name), depth + 1, cancel, observe)?;
            }
        }
        let entry = SourceEntry {
            metadata,
            children,
            link,
        };
        entry.verify(path, cancel)?;
        self.entries.insert(path.to_path_buf(), entry);
        Ok(())
    }

    fn verify(
        &self,
        cancel: &Cancellation,
        mut failed: impl FnMut(&Path, &io::Error),
    ) -> io::Result<()> {
        for (path, entry) in &self.entries {
            if let Err(error) = entry.verify(path, cancel) {
                failed(path, &error);
                return Err(error);
            }
        }
        Ok(())
    }
}

fn copy_tree(
    source: &Path,
    staged: &Path,
    target: &Path,
    progress: &mut Progress,
    manifest: &SourceManifest,
    after_entry: &mut impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let result =
        copy_tree_entry(source, staged, target, progress, manifest, after_entry).map_err(|error| {
            if error
                .get_ref()
                .is_some_and(|cause| cause.is::<AccessError>() || cause.is::<Failure>())
            {
                error
            } else {
                source_io(source, AccessAction::CopyEntry, error)
            }
        });
    progress.results.staged_io(
        &crate::vfs::Location::Local(source.to_path_buf()),
        result.as_ref().err(),
    );
    result
}

fn copy_tree_entry(
    source: &Path,
    staged: &Path,
    target: &Path,
    progress: &mut Progress,
    manifest: &SourceManifest,
    after_entry: &mut impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    progress.checkpoint()?;
    progress.item(source);
    let cancel = progress.cancellation();
    let entry = manifest
        .entries
        .get(source)
        .ok_or_else(|| source_changed(source))?;
    entry.verify(source, &cancel)?;
    let metadata = &entry.metadata;
    let output_path = Location::Local(target.to_owned());
    let output_error = |action, error| access_io(&output_path, action, error);
    if let Some(link) = &entry.link {
        copy_symlink(link, staged, metadata)
            .map_err(|error| output_error(AccessAction::CreateLink, error))?;
    } else if metadata.is_dir() {
        fs::create_dir(staged)
            .map_err(|error| output_error(AccessAction::CreateDirectory, error))?;
        for name in entry.children.as_ref().expect("directory manifest") {
            copy_tree(
                &source.join(name),
                &staged.join(name),
                &target.join(name),
                progress,
                manifest,
                after_entry,
            )?;
        }
        cancel.check()?;
        fs::set_permissions(staged, metadata.permissions())
            .map_err(|error| output_error(AccessAction::WritePermissions, error))?;
    } else if metadata.is_file() {
        let input_path = Location::Local(source.to_owned());
        let input_error = |action, error| access_io(&input_path, action, error);
        let mut input = open_regular_source(source)?;
        let current = input
            .metadata()
            .map_err(|error| input_error(AccessAction::ReadOpenMetadata, error))?;
        entry.verify_metadata(source, &current)?;
        cancel.check()?;
        let mut output = OpenOptions::new()
            .read(progress.copy_verification())
            .write(true)
            .create_new(true)
            .open(staged)
            .map_err(|error| output_error(AccessAction::CreateFile, error))?;
        let (written, source_hash) = if progress.copy_verification() {
            let mut reader = CopyReader::new(
                AccessStream::new(&mut input, &input_path).take(metadata.len()),
                true,
            );
            let mut writer = AccessStream::new(&mut output, &output_path);
            let written = progress.copy(source, Some(metadata.len()), &mut reader, &mut writer)?;
            (written, reader.finish())
        } else {
            #[cfg(target_os = "macos")]
            let written = progress.copy_local(
                source,
                metadata.len(),
                &mut AccessStream::new(&mut input, &input_path),
                &mut AccessStream::new(&mut output, &output_path),
            )?;
            #[cfg(not(target_os = "macos"))]
            let written = progress.copy_local(source, metadata.len(), &mut input, &mut output)?;
            (written, None)
        };
        if written != metadata.len() {
            return Err(CopyVerificationError::SourceSize {
                source: input_path,
                expected: metadata.len(),
                actual: written,
            }
            .into());
        }
        let current = input
            .metadata()
            .map_err(|error| input_error(AccessAction::ReadOpenMetadata, error))?;
        entry.verify_metadata(source, &current)?;
        entry.verify(source, &cancel)?;
        cancel.check()?;
        output
            .sync_all()
            .map_err(|error| output_error(AccessAction::SyncFile, error))?;
        after_entry(source)?;
        if let Some(source_hash) = source_hash {
            output
                .rewind()
                .map_err(|error| output_error(AccessAction::SeekFile, error))?;
            progress.verify_copy(
                source,
                &output_path,
                metadata.len(),
                source_hash,
                &mut AccessStream::new(&mut output, &output_path),
            )?;
        }
        if let Ok(modified) = metadata.modified() {
            output
                .set_times(fs::FileTimes::new().set_modified(modified))
                .map_err(|error| output_error(AccessAction::WriteMetadata, error))?;
        }
        output
            .set_permissions(metadata.permissions())
            .map_err(|error| output_error(AccessAction::WritePermissions, error))?;
        return Ok(());
    } else {
        return Err(unsupported_copy_source(source));
    }
    after_entry(source)
}

pub(crate) fn open_regular_source(source: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    // Refuse a regular file changed into a symlink between inspection and open.
    // Nonblocking open also prevents hanging if it has become a FIFO. This does
    // not create a snapshot or protect against concurrent parent replacement.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let input = options
        .open(source)
        .map_err(|error| source_io(source, AccessAction::OpenFile, error))?;
    let metadata = input
        .metadata()
        .map_err(|error| source_io(source, AccessAction::ReadOpenMetadata, error))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::other(Failure::from(
            ScanError::ExpectedRegularFile(source.to_owned().into()),
        )));
    }
    Ok(input)
}

#[cfg(windows)]
pub(crate) fn same_windows_file(source: &Path, target: &Path) -> bool {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
    };

    // Metadata access is sufficient. Keep both handles open during comparison
    // and do not follow a final-component symlink changed since inspection.
    let mut options = OpenOptions::new();
    options
        .access_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let (Ok(source), Ok(target)) = (options.open(source), options.open(target)) else {
        return false;
    };
    let file_id = |file: &fs::File| {
        let mut info = FILE_ID_INFO::default();
        // SAFETY: the open handle is valid and the output buffer matches FileIdInfo.
        let result = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                (&raw mut info).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        };
        let id = info.FileId.Identifier;
        // All zeros and all ones denote unavailable identifiers.
        (result != 0 && id != [0; 16] && id != [u8::MAX; 16])
            .then_some((info.VolumeSerialNumber, id))
    };
    file_id(&source).is_some_and(|id| file_id(&target) == Some(id))
}

#[cfg(unix)]
fn copy_symlink(target: &Path, destination: &Path, _: &fs::Metadata) -> io::Result<()> {
    std::os::unix::fs::symlink(target, destination)
}

#[cfg(windows)]
fn copy_symlink(target: &Path, destination: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::windows::fs::FileTypeExt;
    if metadata.file_type().is_symlink_dir() {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    }
}

#[cfg(not(any(unix, windows)))]
fn copy_symlink(_: &Path, _: &Path, _: &fs::Metadata) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        crate::i18n::text!("Копирование символических ссылок на этой платформе не поддерживается"),
    ))
}

/// Restore permissions only inside an unpublished private copy before cleanup;
/// never follow a symlink or change any source permissions.
fn remove_staging(staging: &StagingDirectory) -> Result<(), TransferError> {
    staging.ensure_owned()?;
    fn make_removable(path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            return Ok(());
        }
        if metadata.is_dir() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(
                    path,
                    fs::Permissions::from_mode(metadata.permissions().mode() | 0o700),
                )?;
            }
            for child in fs::read_dir(path)? {
                make_removable(&child?.path())?;
            }
        }
        #[cfg(windows)]
        // On Windows this clears the read-only attribute; it does not change Unix modes.
        #[allow(clippy::permissions_set_readonly_false)]
        if metadata.permissions().readonly() {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions)?;
        }
        Ok(())
    }
    let location = Location::Local(staging.path.clone());
    let cleanup = |error| AccessError::new(&location, AccessAction::RemoveStagingDirectory, error);
    make_removable(&staging.path).map_err(cleanup)?;
    staging.ensure_owned()?;
    fs::remove_dir_all(&staging.path).map_err(|error| cleanup(error).into())
}

pub(crate) fn rename_error(from: &Path, to: &Path, source: io::Error) -> TransferError {
    MutationError::RenameLocal {
        from: from.to_owned(),
        to: to.to_owned(),
        source,
    }
    .into()
}

pub(crate) fn rename_with_context(source: &Path, destination: &Path) -> Result<(), TransferError> {
    rename_no_replace(source, destination).map_err(|error| rename_error(source, destination, error))
}

#[cfg(target_os = "macos")]
pub(crate) fn rename_between_open_directories(
    source_parent: &fs::File,
    destination_parent: &fs::File,
    source: &Path,
    destination: &Path,
) -> Result<(), TransferError> {
    use rustix::fs::{renameat_with, RenameFlags};

    let name = |path: &Path| MutationError::RenameRoot(path.to_owned().into());
    let from = source.file_name().ok_or_else(|| name(source))?;
    let to = destination.file_name().ok_or_else(|| name(destination))?;
    renameat_with(
        source_parent,
        from,
        destination_parent,
        to,
        RenameFlags::NOREPLACE,
    )
    .map_err(|error| rename_error(source, destination, error.into()))
}

// Native no-replacement renames close the check-then-rename overwrite race.
// If a platform/filesystem does not support them, fail without a risky fallback.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;
    let invalid_path = |error| io::Error::new(io::ErrorKind::InvalidInput, error);
    let source = CString::new(source.as_os_str().as_bytes()).map_err(invalid_path)?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(invalid_path)?;
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn renamex_np(source: *const c_char, destination: *const c_char, flags: c_uint) -> c_int;
    }
    #[cfg(target_os = "linux")]
    unsafe extern "C" {
        fn renameat2(
            oldfd: c_int,
            source: *const c_char,
            newfd: c_int,
            destination: *const c_char,
            flags: c_uint,
        ) -> c_int;
    }
    // SAFETY: Both CString buffers are valid and terminated for the duration of
    // this synchronous call. These APIs retain no pointers.
    #[cfg(target_os = "macos")]
    let result = unsafe { renamex_np(source.as_ptr(), destination.as_ptr(), 0x4) }; // RENAME_EXCL
    #[cfg(target_os = "linux")]
    let result = unsafe { renameat2(-100, source.as_ptr(), -100, destination.as_ptr(), 1) }; // AT_FDCWD, RENAME_NOREPLACE
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(crate) fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(source: *const u16, destination: *const u16, flags: u32) -> i32;
    }
    let mut source: Vec<u16> = source.as_os_str().encode_wide().collect();
    let mut destination: Vec<u16> = destination.as_os_str().encode_wide().collect();
    if source.contains(&0) || destination.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            crate::i18n::text!("Путь содержит нулевой символ"),
        ));
    }
    source.push(0);
    destination.push(0);
    // SAFETY: Both UTF-16 buffers remain valid and NUL-terminated throughout the
    // call. Zero flags forbids both replacing and cross-volume copy/delete.
    let result = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0) };
    if result != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) fn rename_no_replace(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        crate::i18n::text!(
            "Безопасное переименование без перезаписи на этой платформе пока не реализовано"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn local_copy_keeps_the_original_errno_and_nested_path_for_a_bare_io_failure() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let child = source.join("nested/child");
        let target = root.path().join("target");
        fs::create_dir_all(child.parent().unwrap()).unwrap();
        fs::write(&child, b"copy input").unwrap();
        fs::write(&target, b"existing output").unwrap();
        let error = copy_atomically_cancellable(
            &source,
            &target,
            &mut Progress::default(),
            |_, _| panic!("a failed copy must stop before publication"),
            &mut |path| {
                if path == child {
                    Err(io::Error::from_raw_os_error(libc::EIO))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert!(
            matches!(&error, TransferError::Failed(Failure::Access(access))
            if access.location.local() == Some(child.as_path())
                && access.action == AccessAction::CopyEntry
                && matches!(&access.cause, AccessCause::Local(source) if source.raw_os_error() == Some(libc::EIO)))
        );
        assert!(crate::i18n::file_error(&error).contains(&*child.to_string_lossy()));
        assert_eq!(fs::read(&child).unwrap(), b"copy input");
        assert_eq!(fs::read(&target).unwrap(), b"existing output");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn invalid_rename_path_keeps_the_original_null_byte_cause() {
        use std::os::unix::ffi::OsStrExt;

        let source = Path::new("/fixture/from");
        let target = Path::new(std::ffi::OsStr::from_bytes(b"/fixture/bad\0name"));
        let error = rename_no_replace(source, target).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let cause = error
            .get_ref()
            .unwrap()
            .downcast_ref::<std::ffi::NulError>()
            .unwrap();
        assert_eq!(cause.nul_position(), b"/fixture/bad".len());
        let error = rename_error(source, target, error);
        let message = crate::i18n::file_error(&error);
        assert!(message.contains("/fixture/from"));
        assert!(message.contains(crate::i18n::text!("Путь содержит нулевой символ")));
    }

    #[test]
    fn copy_hash_mismatch_preserves_source_old_target_and_cleans_staging() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::write(&source, b"source bytes").unwrap();
        fs::write(&target, b"old destination").unwrap();
        let mut progress = Progress::default();
        progress.set_copy_verification(true);
        let result = copy_atomically_cancellable(
            &source,
            &target,
            &mut progress,
            |_, _| panic!("hash mismatch must not publish"),
            &mut |_| {
                let staging = fs::read_dir(root.path())
                    .unwrap()
                    .map(Result::unwrap)
                    .find(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".pawcommander-copy-")
                    })
                    .unwrap();
                fs::write(staging.path().join("payload"), b"broken bytes")
            },
        );
        assert!(result.unwrap_err().to_string().contains("SHA-256"));
        assert_eq!(fs::read(&source).unwrap(), b"source bytes");
        assert_eq!(fs::read(&target).unwrap(), b"old destination");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn copy_failure_preserves_a_directory_replacing_its_staging_path() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        let displaced = root.path().join("displaced-staging");
        fs::write(&source, b"source bytes").unwrap();
        let mut replacement = None;
        let result = copy_atomically_checked(
            &source,
            &destination,
            &mut Progress::default(),
            |_, _| panic!("failed copy must not publish"),
            &mut |_| {
                let staging = fs::read_dir(root.path())?
                    .map(Result::unwrap)
                    .find(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".pawcommander-copy-")
                    })
                    .ok_or_else(|| io::Error::other("staging path not found"))?
                    .path();
                fs::rename(&staging, &displaced)?;
                fs::create_dir(&staging)?;
                fs::write(staging.join("foreign"), b"not owned by the copy")?;
                replacement = Some(staging);
                Ok(())
            },
        );
        assert!(result.is_err());
        let replacement = replacement.expect("copy hook ran");
        assert_eq!(
            fs::read(replacement.join("foreign")).unwrap(),
            b"not owned by the copy"
        );
        assert_eq!(
            fs::read(displaced.join("payload")).unwrap(),
            b"source bytes"
        );
        assert_eq!(fs::read(source).unwrap(), b"source bytes");
        assert!(!destination.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_cleanup_preserves_replaced_staging_directory() {
        let root = tempfile::tempdir().unwrap();
        let staging = create_staging_directory(root.path()).unwrap();
        let displaced = root.path().join("displaced-staging");
        let original_id = staging.original_id;
        fs::write(staging.path.join("payload"), b"owned copy").unwrap();
        fs::rename(&staging.path, &displaced).unwrap();
        fs::create_dir(&staging.path).unwrap();
        fs::write(staging.path.join("foreign"), b"not owned by the copy").unwrap();
        let replacement = crate::windows_file_id::from_path(&staging.path)
            .unwrap()
            .expect("replacement file ID");
        assert_eq!(replacement.volume, original_id.volume);
        assert_ne!(replacement.id, original_id.id);

        assert!(remove_staging(&staging).is_err());
        assert_eq!(
            fs::read(staging.path.join("foreign")).unwrap(),
            b"not owned by the copy"
        );
        assert_eq!(fs::read(displaced.join("payload")).unwrap(), b"owned copy");
    }

    #[test]
    fn stop_during_copy_hash_verification_preserves_existing_destination() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        let bytes = vec![0x5a; 256 * 1024];
        fs::write(&source, &bytes).unwrap();
        fs::write(&target, b"old destination").unwrap();
        let armed = Arc::new(AtomicBool::new(false));
        let watch = armed.clone();
        let mut verification_checks = 0;
        let cancel = Cancellation::at_checkpoint(move || {
            if watch.load(Ordering::Relaxed) {
                verification_checks += 1;
            }
            verification_checks == 3
        });
        let mut progress = Progress::with_cancellation(cancel);
        progress.set_copy_verification(true);
        let result = copy_atomically_cancellable(
            &source,
            &target,
            &mut progress,
            |_, _| panic!("stopped verification must not publish"),
            &mut |_| {
                armed.store(true, Ordering::Relaxed);
                Ok(())
            },
        );
        assert!(matches!(result, Err(TransferError::Cancelled)));
        assert_eq!(fs::read(source).unwrap(), bytes);
        assert_eq!(fs::read(target).unwrap(), b"old destination");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn copy_hash_verification_precedes_restrictive_destination_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("readonly"), b"read only bytes").unwrap();
        fs::write(source.join("empty"), []).unwrap();
        std::os::unix::fs::symlink("missing", source.join("link")).unwrap();
        fs::set_permissions(source.join("readonly"), fs::Permissions::from_mode(0o400)).unwrap();
        let mut progress = Progress::default();
        progress.set_copy_verification(true);
        copy_atomically_cancellable(
            &source,
            &target,
            &mut progress,
            |payload, target| fs::rename(payload, target).map_err(Into::into),
            &mut |_| Ok(()),
        )
        .unwrap();
        assert_eq!(
            fs::read(target.join("readonly")).unwrap(),
            b"read only bytes"
        );
        assert_eq!(
            fs::metadata(target.join("readonly"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o400
        );
        assert_eq!(fs::metadata(target.join("empty")).unwrap().len(), 0);
        assert_eq!(
            fs::read_link(target.join("link")).unwrap(),
            PathBuf::from("missing")
        );
    }

    #[test]
    fn cancellation_during_local_stream_preserves_replacement_and_removes_staging() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        let bytes = vec![0x5a; 3 * 1024 * 1024];
        fs::write(&source, &bytes).unwrap();
        fs::write(&target, b"previous destination").unwrap();
        let directory = root.path().to_path_buf();
        let cancel = Cancellation::at_checkpoint(move || {
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".pawcommander-copy-")
                        && fs::metadata(entry.path().join("payload"))
                            .is_ok_and(|meta| meta.len() >= 1024 * 1024)
                })
        });
        let mut progress = Progress::with_cancellation(cancel);
        let error = copy_atomically_cancellable(
            &source,
            &target,
            &mut progress,
            |_, _| panic!("cancelled staging must not publish"),
            &mut |_| Ok(()),
        )
        .unwrap_err();
        assert!(matches!(error, TransferError::Cancelled));
        assert_eq!(fs::read(&source).unwrap(), bytes);
        assert_eq!(fs::read(&target).unwrap(), b"previous destination");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn cancellation_in_source_capture_and_final_verification_prevents_publication() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        for index in 0..32 {
            fs::write(source.join(format!("file-{index:02}")), b"data").unwrap();
        }
        let mut checks = 0;
        let cancel = Cancellation::at_checkpoint(move || {
            checks += 1;
            checks == 12
        });
        let error = SourceManifest::capture_cancellable(&source, &cancel)
            .err()
            .unwrap();
        assert!(matches!(
            TransferError::from(error),
            TransferError::Cancelled
        ));

        let copied = Arc::new(AtomicBool::new(false));
        let checkpoint_copied = copied.clone();
        let mut verification_checks = 0;
        let cancel = Cancellation::at_checkpoint(move || {
            if checkpoint_copied.load(Ordering::Relaxed) {
                verification_checks += 1;
            }
            verification_checks == 5
        });
        let mut progress = Progress::with_cancellation(cancel);
        let error = copy_atomically_cancellable(
            &source,
            &target,
            &mut progress,
            |_, _| panic!("final verification cancellation must not publish"),
            &mut |path| {
                if path == source {
                    copied.store(true, Ordering::Relaxed);
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            copied.load(Ordering::Relaxed),
            "the complete tree was staged first"
        );
        assert!(matches!(error, TransferError::Cancelled));
        assert!(!target.exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_cleans_readonly_staged_tree_without_following_links() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        let outside = root.path().join("outside");
        fs::create_dir(&source).unwrap();
        fs::write(&outside, b"outside sentinel").unwrap();
        fs::write(source.join("data"), b"content").unwrap();
        symlink(&outside, source.join("external")).unwrap();
        symlink(root.path().join("missing"), source.join("broken")).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o500)).unwrap();
        let mut progress = Progress::default();
        let cancel = progress.cancellation();
        let result = copy_atomically_cancellable(
            &source,
            &target,
            &mut progress,
            |_, _| panic!("stopped before publication"),
            &mut |path| {
                if path == source {
                    cancel.request();
                }
                Ok(())
            },
        );
        assert!(matches!(result, Err(TransferError::Cancelled)));
        assert_eq!(fs::read(&outside).unwrap(), b"outside sentinel");
        assert_eq!(
            fs::metadata(&source).unwrap().permissions().mode() & 0o777,
            0o500
        );
        assert!(!target.exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn source_file_changes_abort_before_publication_and_clean_staging() {
        for mutation in ["append", "truncate", "rewrite", "remove", "directory"] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("source");
            let destination = root.path().join("destination");
            fs::write(&source, b"original").unwrap();
            fs::write(&destination, b"previous target").unwrap();
            let mut published = false;
            let result = copy_atomically_cancellable(
                &source,
                &destination,
                &mut Progress::default(),
                |_, _| {
                    published = true;
                    Ok(())
                },
                &mut |path| {
                    assert_eq!(path, source);
                    match mutation {
                        "append" => fs::write(path, b"original with extra bytes")?,
                        "truncate" => fs::write(path, b"short")?,
                        "rewrite" => {
                            fs::write(path, b"modified")?;
                            // Deterministic even on a coarse timestamp filesystem.
                            fs::File::options()
                                .write(true)
                                .open(path)?
                                .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
                        }
                        "remove" => fs::remove_file(path)?,
                        "directory" => {
                            fs::remove_file(path)?;
                            fs::create_dir(path)?;
                        }
                        _ => unreachable!(),
                    }
                    Ok(())
                },
            );
            let error = result.unwrap_err();
            if mutation == "remove" {
                let TransferError::Failed(Failure::Access(error)) = error else {
                    panic!("expected the original metadata failure, got {error:?}");
                };
                assert_eq!(error.location.path(), source);
                assert_eq!(error.action, AccessAction::ReadMetadata);
                assert!(
                    matches!(&error.cause, AccessCause::Local(cause) if cause.kind() == io::ErrorKind::NotFound)
                );
            } else {
                assert!(
                    matches!(&error, TransferError::Failed(Failure::Mutation(reason))
                    if matches!(reason.as_ref(), MutationError::SourceChanged(location) if location.path() == source)),
                    "{mutation}: {error}"
                );
            }
            assert!(!published, "{mutation}");
            assert_eq!(fs::read(&destination).unwrap(), b"previous target");
            assert!(!fs::read_dir(root.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pawcommander-copy-")
            }));
            match mutation {
                "append" => assert_eq!(fs::read(source).unwrap(), b"original with extra bytes"),
                "truncate" => assert_eq!(fs::read(source).unwrap(), b"short"),
                "rewrite" => assert_eq!(fs::read(source).unwrap(), b"modified"),
                "remove" => assert!(!source.exists()),
                "directory" => assert!(source.is_dir()),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn final_tree_check_revisits_early_files_and_directory_membership() {
        for mutation in ["early-file", "add", "remove", "rename"] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("source");
            let destination = root.path().join("destination");
            fs::create_dir_all(source.join("a-nested")).unwrap();
            let early = source.join("a-nested/early");
            let late = source.join("z-late");
            fs::write(&early, b"early").unwrap();
            fs::write(&late, b"late").unwrap();
            fs::create_dir(&destination).unwrap();
            fs::write(destination.join("precious"), b"previous target").unwrap();
            let result = copy_atomically_checked(
                &source,
                &destination,
                &mut Progress::default(),
                |_, _| panic!("changed tree must never be published"),
                &mut |path| {
                    // The nested directory and its child have already been
                    // copied. Checking only each file at read time misses this.
                    if path == late {
                        match mutation {
                            "early-file" => fs::write(&early, b"changed early file")?,
                            "add" => fs::write(source.join("a-nested/new"), b"new")?,
                            "remove" => fs::remove_file(&early)?,
                            "rename" => fs::rename(&early, source.join("a-nested/renamed"))?,
                            _ => unreachable!(),
                        }
                    }
                    Ok(())
                },
            );
            assert!(
                result.unwrap_err().contains("changed during copying"),
                "{mutation}"
            );
            assert_eq!(
                fs::read(destination.join("precious")).unwrap(),
                b"previous target"
            );
            assert_eq!(fs::read(&late).unwrap(), b"late");
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
        }
    }

    #[cfg(unix)]
    #[test]
    fn source_identity_is_checked_even_when_size_and_mtime_match() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::write(&source, b"original").unwrap();
        let manifest = SourceManifest::capture(&source).unwrap();
        let entry = &manifest.entries[&source];
        let replacement = root.path().join("replacement");
        fs::write(&replacement, b"replaced").unwrap();
        fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(entry.metadata.modified().unwrap()))
            .unwrap();
        fs::rename(&replacement, &source).unwrap();
        let opened = open_regular_source(&source).unwrap();
        assert!(entry
            .verify_metadata(&source, &opened.metadata().unwrap())
            .is_err());
        assert!(manifest
            .verify(&Cancellation::default(), |_, _| {})
            .is_err());
        assert_eq!(fs::read(source).unwrap(), b"replaced");
    }

    #[cfg(unix)]
    #[test]
    fn final_tree_check_detects_retargeted_symlink_without_touching_targets() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        let outside = root.path().join("outside");
        fs::create_dir(&source).unwrap();
        fs::write(&outside, b"precious").unwrap();
        let link = source.join("link");
        std::os::unix::fs::symlink("missing", &link).unwrap();
        let result = copy_atomically_checked(
            &source,
            &destination,
            &mut Progress::default(),
            |_, _| panic!("changed link must never be published"),
            &mut |path| {
                if path == source {
                    fs::remove_file(&link)?;
                    std::os::unix::fs::symlink(&outside, &link)?;
                }
                Ok(())
            },
        );
        assert!(result.unwrap_err().contains("changed during copying"));
        assert!(!destination.exists());
        assert_eq!(fs::read_link(link).unwrap(), outside);
        assert_eq!(fs::read(outside).unwrap(), b"precious");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn space_exhaustion_after_preflight_preserves_files_and_cleans_staging() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let folder = root.path().join("destination folder");
        fs::create_dir(&folder).unwrap();
        let destination = folder.join("existing");
        fs::write(&source, b"original").unwrap();
        fs::write(&destination, b"previous target").unwrap();

        let error = copy_atomically_checked(
            &source,
            &destination,
            &mut Progress::default(),
            |_, _| panic!("an incomplete copy must never be published"),
            &mut |_| Err(io::ErrorKind::StorageFull.into()),
        )
        .unwrap_err();

        assert!(error.contains(folder.to_string_lossy().as_ref()), "{error}");
        assert_eq!(fs::read(source).unwrap(), b"original");
        assert_eq!(fs::read(destination).unwrap(), b"previous target");
        assert_eq!(fs::read_dir(folder).unwrap().count(), 1);
    }

    #[test]
    fn space_preflight_is_conservative_and_does_not_modify_files() {
        use crate::file_error::Failure;

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::write(&source, b"new contents").unwrap();
        fs::write(&destination, b"existing contents").unwrap();
        check_local_copy_space(SourceManifest::capture(&source).unwrap().bytes, root.path())
            .unwrap();
        let folder = Location::Local(root.path().to_owned());
        let error = ensure_copy_space(12, 11, &folder).unwrap_err();
        assert!(matches!(
            error,
            TransferError::Failed(Failure::Space(reason))
                if matches!(reason.as_ref(), SpaceError::Insufficient {
                    directory, required: 12, available: 11,
                } if directory == &folder)
        ));
        assert!(ensure_copy_space(12, 12, &folder).is_ok());
        assert!(ensure_copy_space(u64::MAX, u128::from(u64::MAX) + 1, &folder).is_ok());
        let missing = root.path().join("missing");
        let error = check_local_copy_space(1, &missing).unwrap_err();
        let TransferError::Failed(Failure::Space(reason)) = error else {
            panic!("expected a typed space query error");
        };
        let SpaceError::LocalQuery {
            directory,
            source: cause,
        } = reason.as_ref()
        else {
            panic!("expected failure to query the local folder");
        };
        assert_eq!(directory, &missing);
        assert_eq!(cause.kind(), io::ErrorKind::NotFound);
        #[cfg(unix)]
        assert_eq!(cause.raw_os_error(), Some(libc::ENOENT));
        assert_eq!(fs::read(source).unwrap(), b"new contents");
        assert_eq!(fs::read(destination).unwrap(), b"existing contents");
    }
    #[test]
    fn space_estimate_includes_nested_files_and_full_replacement() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("one"), b"1234").unwrap();
        fs::write(source.join("nested/two"), b"56789").unwrap();
        assert_eq!(SourceManifest::capture(&source).unwrap().bytes, 9);
        let bytes = SourceManifest::capture(&source).unwrap().bytes;
        let folder = Location::Local(root.path().to_owned());
        assert!(ensure_copy_space(bytes, 8, &folder).is_err());
        assert!(ensure_copy_space(bytes, 9, &folder).is_ok());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&source, source.join("link")).unwrap();
            assert_eq!(SourceManifest::capture(&source).unwrap().bytes, 9);
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_copy_distinguishes_disk_full_and_quota_with_original_errors() {
        use crate::file_error::Failure;

        let folder = Path::new("/space-test");
        for (errno, kind) in [
            (libc::ENOSPC, io::ErrorKind::StorageFull),
            (libc::EDQUOT, io::ErrorKind::QuotaExceeded),
        ] {
            let error = local_copy_error(Some(folder), io::Error::from_raw_os_error(errno));
            let TransferError::Failed(Failure::Space(reason)) = error else {
                panic!("expected a typed local write error");
            };
            assert!(matches!(
                reason.as_ref(), SpaceError::LocalWrite { directory, source }
                    if directory == folder && source.kind() == kind && source.raw_os_error() == Some(errno)
            ));
        }
        let source = Location::Local("/space-test/source".into());
        let error = access_io(
            &source,
            AccessAction::ReadFile,
            io::Error::from_raw_os_error(libc::EDQUOT),
        );
        let error = local_copy_error(Some(folder), error);
        let TransferError::Failed(Failure::Access(error)) = error else {
            panic!("a source failure must keep its read context");
        };
        assert_eq!(error.location, source);
        assert_eq!(error.action, AccessAction::ReadFile);

        let destination = Location::Local(folder.join("nested-folder"));
        for action in [
            AccessAction::CreateDirectory,
            AccessAction::CreateLink,
            AccessAction::WritePermissions,
        ] {
            let error = local_copy_write_error(
                &destination,
                action,
                io::Error::from_raw_os_error(libc::ENOSPC),
            );
            let TransferError::Failed(Failure::Space(reason)) = error else {
                panic!("expected a typed local capacity error");
            };
            let SpaceError::LocalWrite { directory, source } = reason.as_ref() else {
                panic!("expected a local write error");
            };
            let access = source
                .get_ref()
                .unwrap()
                .downcast_ref::<AccessError>()
                .unwrap();
            assert_eq!(directory, folder);
            assert_eq!(access.location, destination);
            assert_eq!(access.action, action);
            assert!(
                matches!(&access.cause, AccessCause::Local(error) if error.raw_os_error() == Some(libc::ENOSPC))
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn failed_staged_link_creation_reports_the_final_child_and_preserves_existing_data() {
        let fixture = tempdir().unwrap();
        let root = fixture.path();
        let source = root.join("source");
        let target = root.join("target");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("a-file"), b"incoming").unwrap();
        std::os::unix::fs::symlink("a-file", source.join("z-link")).unwrap();
        fs::write(&target, b"previous destination").unwrap();

        let error = copy_atomically_cancellable(
            &source,
            &target,
            &mut Progress::default(),
            |_, _| panic!("copy with a failed link must not publish"),
            &mut |path| {
                if path == source.join("a-file") {
                    let stage = fs::read_dir(root)?
                        .map(Result::unwrap)
                        .find(|entry| {
                            entry
                                .file_name()
                                .to_string_lossy()
                                .starts_with(".pawcommander-copy-")
                        })
                        .unwrap();
                    fs::write(stage.path().join("payload/z-link"), b"occupied")?;
                }
                Ok(())
            },
        )
        .unwrap_err();
        let TransferError::Failed(Failure::Access(error)) = error else {
            panic!("expected the original destination failure, got {error:?}");
        };
        assert_eq!(error.location.path(), target.join("z-link"));
        assert_eq!(error.action, AccessAction::CreateLink);
        assert!(
            matches!(error.cause, AccessCause::Local(error) if error.raw_os_error() == Some(libc::EEXIST))
        );
        assert_eq!(fs::read(&target).unwrap(), b"previous destination");
        assert_eq!(
            fs::read_link(source.join("z-link")).unwrap(),
            Path::new("a-file")
        );
        assert_eq!(fs::read(source.join("a-file")).unwrap(), b"incoming");
        assert_eq!(fs::read_dir(root).unwrap().count(), 2);
    }

    use tempfile::tempdir;

    #[test]
    fn listing_keeps_hidden_files_and_orders_directories_first() {
        let root = tempdir().unwrap();
        fs::write(root.path().join(".hidden"), b"data").unwrap();
        fs::write(root.path().join("B.txt"), b"hi").unwrap();
        fs::create_dir(root.path().join("z folder")).unwrap();
        let entries = read_directory(root.path(), &|| false).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["z folder", ".hidden", "B.txt"]
        );
        assert!(entries[0].is_dir);
        assert_eq!(entries[2].size, 2);
    }

    #[test]
    fn names_cannot_escape_the_parent_or_replace_an_existing_item() {
        let root = tempdir().unwrap();
        for name in [
            "",
            " ",
            ".",
            "..",
            "../escape",
            "/absolute",
            "a/b",
            "a\\b",
            "a\0b",
        ] {
            assert!(create_directory(root.path(), name).is_err(), "{name:?}");
        }
        let folder = create_directory(root.path(), "Папка с пробелом").unwrap();
        assert!(folder.is_dir());
        assert!(create_directory(root.path(), "Папка с пробелом").is_err());
        fs::write(root.path().join("existing"), b"keep").unwrap();
        assert!(rename_entry(&folder, "existing").is_err());
        assert_eq!(fs::read(root.path().join("existing")).unwrap(), b"keep");
    }

    #[test]
    fn recursive_copy_normalizes_parent_child_and_duplicate_selections() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::create_dir(&destination).unwrap();
        let child = source.join("nested/file.txt");
        fs::write(&child, b"contents").unwrap();
        assert_eq!(
            transfer(
                &[child, source.clone(), source.clone()],
                &destination,
                TransferKind::Copy
            )
            .unwrap(),
            1
        );
        assert_eq!(
            fs::read(destination.join("source/nested/file.txt")).unwrap(),
            b"contents"
        );
        assert!(source.exists());
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
    }

    #[test]
    fn refuses_copy_and_move_into_descendants_without_changes() {
        let root = tempdir().unwrap();
        let source: Location = root.path().join("source").into();
        let nested = source.join("nested");
        fs::create_dir_all(nested.path()).unwrap();
        for kind in [TransferKind::Copy, TransferKind::Move] {
            for destination in [&source, &nested] {
                let failure = crate::vfs::transfer_with_conflicts(
                    std::slice::from_ref(&source),
                    destination,
                    kind,
                    &mut Progress::default(),
                    |_| panic!("transfer into the source must fail before conflicts"),
                    |_, _| panic!("transfer into the source must fail before publication"),
                )
                .unwrap_err();
                assert!(failure.summary.preparing);
                assert!(
                    matches!(failure.error, TransferError::Failed(crate::file_error::Failure::TransferPlan(reason))
                    if matches!(reason.as_ref(), TransferPlanError::IntoSource { source: original, destination: chosen }
                        if original == &source && chosen == destination))
                );
            }
        }
        assert!(source.path().is_dir());
        assert_eq!(fs::read_dir(nested.path()).unwrap().count(), 0);
    }

    #[test]
    fn preflights_all_conflicts_before_starting_a_batch() {
        let root = tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        let target = root.path().join("target");
        fs::write(&first, b"one").unwrap();
        fs::write(&second, b"two").unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("second"), b"original").unwrap();
        assert!(transfer(
            &[first.clone(), second.clone()],
            &target,
            TransferKind::Move
        )
        .is_err());
        assert!(first.exists());
        assert!(second.exists());
        assert!(!target.join("first").exists());
        assert_eq!(fs::read(target.join("second")).unwrap(), b"original");
    }

    #[test]
    fn moves_and_renames_without_overwriting() {
        let root = tempdir().unwrap();
        let source = root.path().join("original");
        fs::write(&source, b"keep bytes").unwrap();
        let renamed = rename_entry(&source, "renamed").unwrap();
        assert!(!source.exists());
        let destination = create_directory(root.path(), "destination").unwrap();
        assert_eq!(
            transfer(
                std::slice::from_ref(&renamed),
                &destination,
                TransferKind::Move
            )
            .unwrap(),
            1
        );
        assert!(!renamed.exists());
        assert_eq!(
            fs::read(destination.join("renamed")).unwrap(),
            b"keep bytes"
        );
        fs::write(&source, b"other").unwrap();
        assert!(rename_no_replace(&source, &destination.join("renamed")).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"other");
        assert_eq!(
            fs::read(destination.join("renamed")).unwrap(),
            b"keep bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copies_symlinks_without_following_them_including_broken_links() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let source = create_directory(root.path(), "source").unwrap();
        let destination = create_directory(root.path(), "destination").unwrap();
        let outside = create_directory(root.path(), "outside").unwrap();
        fs::write(outside.join("precious"), b"data").unwrap();
        symlink(&outside, source.join("directory-link")).unwrap();
        symlink("missing", source.join("broken-link")).unwrap();
        let entries = read_directory(&source, &|| false).unwrap();
        assert!(entries
            .iter()
            .any(|e| e.name == "directory-link" && e.is_dir && e.is_symlink));
        assert!(entries
            .iter()
            .any(|e| e.name == "broken-link" && !e.is_dir && e.is_symlink));
        transfer(
            std::slice::from_ref(&source),
            &destination,
            TransferKind::Copy,
        )
        .unwrap();
        assert_eq!(
            fs::read_link(destination.join("source/directory-link")).unwrap(),
            outside
        );
        assert_eq!(
            fs::read_link(destination.join("source/broken-link")).unwrap(),
            Path::new("missing")
        );
        assert_eq!(
            fs::read(root.path().join("outside/precious")).unwrap(),
            b"data"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn regular_source_open_refuses_a_replacement_symlink() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let actual = root.path().join("actual");
        let replacement = root.path().join("replacement");
        fs::write(&actual, b"private").unwrap();
        symlink(&actual, &replacement).unwrap();
        let error = TransferError::from(open_regular_source(&replacement).unwrap_err());
        let TransferError::Failed(Failure::Access(error)) = error else {
            panic!("expected the original open failure, got {error:?}");
        };
        assert_eq!(error.location.path(), replacement);
        assert_eq!(error.action, AccessAction::OpenFile);
        assert!(
            matches!(&error.cause, AccessCause::Local(cause) if cause.raw_os_error() == Some(libc::ELOOP))
        );
        assert_eq!(fs::read(actual).unwrap(), b"private");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn regular_source_open_refuses_a_fifo_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::process::Command;
        use std::time::{Duration, Instant};

        const CHILD_FIFO: &str = "PAWCOMMANDER_TEST_REGULAR_SOURCE_FIFO";
        if let Some(fifo) = std::env::var_os(CHILD_FIFO) {
            let error = TransferError::from(open_regular_source(Path::new(&fifo)).unwrap_err());
            assert!(matches!(error, TransferError::Failed(Failure::Scan(reason))
                if matches!(reason.as_ref(), ScanError::ExpectedRegularFile(location) if location.path() == Path::new(&fifo))));
            return;
        }

        let root = tempdir().unwrap();
        let fifo = root.path().join("replacement-fifo");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a live NUL-terminated path inside the test directory.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);

        // A missing O_NONBLOCK would wait forever for a writer. Isolate the
        // open so the parent can terminate it and fail without hanging the suite.
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fs_ops::tests::regular_source_open_refuses_a_fifo_without_blocking",
            ])
            .env(CHILD_FIFO, &fifo)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "FIFO source rejection failed: {status}");
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("opening a FIFO source blocked instead of rejecting it");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn broken_destination_symlinks_are_conflicts_for_copy_and_rename() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let source = root.path().join("file");
        fs::write(&source, b"data").unwrap();
        let destination = create_directory(root.path(), "destination").unwrap();
        symlink("does-not-exist", destination.join("file")).unwrap();
        assert!(transfer(
            std::slice::from_ref(&source),
            &destination,
            TransferKind::Copy
        )
        .is_err());
        assert!(rename_no_replace(&source, &destination.join("file")).is_err());
        assert!(source.exists());
        assert_eq!(
            fs::read_link(destination.join("file")).unwrap(),
            Path::new("does-not-exist")
        );
    }

    #[test]
    fn publication_conflict_cleans_staging_and_leaves_existing_data_intact() {
        let root = tempdir().unwrap();
        let source = create_directory(root.path(), "source").unwrap();
        let destination = create_directory(root.path(), "destination").unwrap();
        fs::write(source.join("regular"), b"keep").unwrap();
        let existing = destination.join("source");
        fs::write(&existing, b"existing destination").unwrap();
        // Call the copying step directly to simulate a destination appearing
        // after preflight. The fully staged copy must be removed on conflict.
        assert!(copy_atomically(&source, &existing).is_err());
        assert_eq!(fs::read_dir(destination).unwrap().count(), 1);
        assert_eq!(fs::read(existing).unwrap(), b"existing destination");
        assert_eq!(fs::read(source.join("regular")).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn descendant_check_resolves_destination_aliases() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let source = create_directory(root.path(), "source").unwrap();
        let nested = create_directory(&source, "nested").unwrap();
        let alias = root.path().join("alias");
        symlink(nested, &alias).unwrap();
        assert!(transfer(&[source], &alias, TransferKind::Copy).is_err());
    }
}
