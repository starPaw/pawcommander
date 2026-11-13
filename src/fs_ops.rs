//! Local filesystem operations. Copies are staged privately on the destination
//! volume before publication. This does not snapshot a changing filesystem.

use crate::operations::Progress;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
) -> Result<Vec<FileEntry>, String> {
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
    mut visit: impl FnMut(FileEntry),
) -> Result<(), String> {
    if cancelled() {
        return Ok(());
    }
    let mut directory = fs::read_dir(path)
        .map_err(|e| at(crate::i18n::text("Не удалось прочитать папку"), path, e))?;
    while !cancelled() {
        let Some(item) = directory.next() else {
            break;
        };
        if cancelled() {
            break;
        }
        let item = item.map_err(|e| {
            at(
                crate::i18n::text("Не удалось прочитать элемент папки"),
                path,
                e,
            )
        })?;
        let path = item.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // Another app may remove a file while the listing is being read.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(at(
                    crate::i18n::text("Не удалось получить сведения об элементе"),
                    &path,
                    e,
                ))
            }
        };
        if cancelled() {
            break;
        }
        let is_symlink = metadata.file_type().is_symlink();
        let target = if is_symlink {
            fs::metadata(&path).ok()
        } else {
            None
        };
        visit(FileEntry {
            name: item.file_name().to_string_lossy().into_owned(),
            path,
            is_dir: target.as_ref().unwrap_or(&metadata).is_dir(),
            is_symlink,
            size: metadata.len(),
            modified: metadata.modified().ok(),
            metadata: ListingMetadata::from_local(&metadata),
        });
    }
    Ok(())
}

pub fn create_directory(parent: &Path, name: &str) -> Result<PathBuf, String> {
    validate_name(name)?;
    let parent = directory_path(parent)?;
    let destination = parent.join(name);
    fs::create_dir(&destination).map_err(|e| {
        at(
            crate::i18n::text("Не удалось создать папку"),
            &destination,
            e,
        )
    })?;
    Ok(destination)
}

pub fn rename_entry(source: &Path, name: &str) -> Result<PathBuf, String> {
    validate_name(name)?;
    let source = entry_path(source)?;
    fs::symlink_metadata(&source).map_err(|e| {
        at(
            crate::i18n::text("Не удалось получить сведения об элементе"),
            &source,
            e,
        )
    })?;
    let destination = source
        .parent()
        .ok_or(crate::i18n::text(
            "Нельзя переименовать корень файловой системы",
        ))?
        .join(name);
    if source == destination {
        return Ok(source);
    }
    ensure_absent(&destination)?;
    rename_no_replace(&source, &destination).map_err(|e| rename_error(&source, &destination, e))?;
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
) -> Result<Vec<LocalTransfer>, String> {
    progress.item(destination);
    let destination = directory_path(destination)?;
    let sources = normalize_selection(sources, progress)?;
    let mut names = HashSet::new();
    let mut plan = Vec::new();
    for LocalSelection {
        original,
        source,
        covered,
        metadata,
    } in sources
    {
        progress.item(&original);
        let current = fs::symlink_metadata(&source).map_err(|e| {
            at(
                crate::i18n::text("Не удалось получить сведения об элементе"),
                &source,
                e,
            )
        })?;
        if current.is_dir() && destination.starts_with(&source) {
            return Err(crate::tr_format!(
                "Нельзя скопировать или переместить «{}» в себя или во вложенную папку",
                "Cannot copy or move “{}” into itself or a subfolder",
                source.display()
            ));
        }
        let name = source.file_name().ok_or(crate::i18n::text(
            "Нельзя скопировать или переместить корень файловой системы",
        ))?;
        if !names.insert(name.to_os_string()) {
            return Err(crate::tr_format!(
                "Несколько выбранных элементов называются «{}»; перенесите их по отдельности",
                "Multiple selected items are named “{}”; transfer them separately",
                name.to_string_lossy()
            ));
        }
        let target = destination.join(name);
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

pub fn trash_entries(paths: &[PathBuf], progress: &mut Progress) -> Result<usize, String> {
    let paths = normalize_selection(paths, progress)?;
    for (completed, entry) in paths.iter().enumerate() {
        let path = &entry.source;
        progress.item(path);
        // Use the operating system's trash. There is deliberately no permanent
        // deletion fallback when trash is unavailable for a volume.
        if let Err(error) = trash::delete(path) {
            return Err(format!(
                "Не удалось переместить «{}» в корзину: {error}. Уже перемещено в корзину: {completed}.",
                path.display()
            ));
        }
    }
    Ok(paths.len())
}

fn validate_name(name: &str) -> Result<(), String> {
    let mut components = Path::new(name).components();
    if name.trim().is_empty()
        || name.contains(['/', '\\', '\0'])
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err("Введите имя файла или папки без / и \\".to_owned());
    }
    Ok(())
}

fn directory_path(path: &Path) -> Result<PathBuf, String> {
    let path = fs::canonicalize(path).map_err(|e| at("Не удалось открыть папку", path, e))?;
    if !path.is_dir() {
        return Err(format!("«{}» не является папкой", path.display()));
    }
    Ok(path)
}

/// Resolve parent aliases but preserve the final component: canonicalizing a
/// selected symlink itself would accidentally operate on the link's target.
fn entry_path(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("Не удалось определить текущую папку: {e}"))?
            .join(path)
    };
    let name = absolute
        .file_name()
        .ok_or("Нельзя изменять корень файловой системы")?;
    let parent = absolute
        .parent()
        .ok_or("Нельзя изменять корень файловой системы")?;
    let parent = fs::canonicalize(parent)
        .map_err(|e| at("Не удалось определить путь к папке", parent, e))?;
    let entry = parent.join(name);
    let metadata = fs::symlink_metadata(&entry)
        .map_err(|e| at("Не удалось получить сведения об элементе", &entry, e))?;
    if metadata.file_type().is_symlink() {
        Ok(entry)
    } else {
        // Resolve the actual spelling on case-insensitive volumes as well as
        // parent aliases, so descendant protection compares canonical paths.
        fs::canonicalize(&entry).map_err(|e| at("Не удалось определить путь к элементу", &entry, e))
    }
}

struct LocalSelection {
    original: PathBuf,
    source: PathBuf,
    covered: Vec<PathBuf>,
    metadata: fs::Metadata,
}

fn normalize_selection(
    paths: &[PathBuf],
    progress: &mut Progress,
) -> Result<Vec<LocalSelection>, String> {
    let mut entries = Vec::with_capacity(paths.len());
    for original in paths {
        progress.item(original);
        let path = entry_path(original)?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| at("Не удалось получить сведения об элементе", &path, e))?;
        entries.push((path, metadata));
    }
    let mut first = HashMap::with_capacity(entries.len());
    for (index, (path, _)) in entries.iter().enumerate() {
        first.entry(path.as_path()).or_insert(index);
    }
    let mut retained = vec![false; entries.len()];
    let mut covered = vec![Vec::new(); entries.len()];
    for (index, (path, _)) in entries.iter().enumerate() {
        // The outermost selected directory covers all selected entries in
        // its tree, including paths spelled through other parent aliases.
        let representative = path
            .ancestors()
            .skip(1)
            .filter_map(|ancestor| first.get(ancestor).copied())
            .filter(|&ancestor| entries[ancestor].1.is_dir())
            .last()
            .unwrap_or(first[path.as_path()]);
        if index == representative {
            retained[index] = true;
        } else {
            covered[representative].push(paths[index].clone());
        }
    }
    Ok(entries
        .into_iter()
        .enumerate()
        .filter(|(index, _)| retained[*index])
        .map(|(index, (source, metadata))| LocalSelection {
            original: paths[index].clone(),
            source,
            covered: std::mem::take(&mut covered[index]),
            metadata,
        })
        .collect())
}

fn ensure_absent(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(format!("«{}» уже существует. Выберите другое имя или папку; существующие элементы не перезаписываются", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(at("Не удалось проверить место назначения", path, e)),
    }
}

static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

fn create_staging_directory(parent: &Path) -> Result<PathBuf, String> {
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
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(copy_space_error(parent, &e).unwrap_or_else(|| {
                    at("Не удалось подготовить копирование в папку", parent, e)
                }))
            }
        }
    }
    Err("Не удалось создать временную папку для копирования; повторите попытку".to_owned())
}

#[cfg(test)]
fn copy_atomically(source: &Path, destination: &Path) -> Result<(), String> {
    copy_atomically_with_progress(
        source,
        destination,
        &mut Progress::default(),
        |payload, destination| {
            rename_no_replace(payload, destination).map_err(|e| {
                format!(
                    "Не удалось завершить копирование «{}» в «{}»: {e}",
                    source.display(),
                    destination.display()
                )
            })
        },
    )
}

pub(crate) fn copy_atomically_with_progress(
    source: &Path,
    destination: &Path,
    progress: &mut Progress,
    publish: impl FnOnce(&Path, &Path) -> Result<(), String>,
) -> Result<(), String> {
    copy_atomically_checked(source, destination, progress, publish, &mut |_| Ok(()))
}

fn copy_atomically_checked(
    source: &Path,
    destination: &Path,
    progress: &mut Progress,
    publish: impl FnOnce(&Path, &Path) -> Result<(), String>,
    after_entry: &mut impl FnMut(&Path) -> io::Result<()>,
) -> Result<(), String> {
    progress.item(source);
    let manifest = SourceManifest::capture(source).map_err(|e| {
        at(
            crate::i18n::text("Не удалось проверить источник"),
            source,
            e,
        )
    })?;
    let parent = destination
        .parent()
        .ok_or("Недопустимое место назначения копии")?;
    ensure_copy_space(
        manifest.bytes,
        available_copy_space(parent)?,
        parent.display(),
    )?;
    let staging = create_staging_directory(parent)?;
    let payload = staging.join("payload");
    let result = copy_tree(source, &payload, progress, &manifest, after_entry)
        // Revisit *every* entry: an early file can change while a later file is
        // being copied without changing the parent directory's metadata.
        .and_then(|()| manifest.verify())
        .map_err(|e| {
            copy_space_error(parent, &e)
                .unwrap_or_else(|| at(crate::i18n::text("Не удалось скопировать"), source, e))
        })
        .and_then(|()| publish(&payload, destination));
    // Only remove our own staging area, never a user's existing destination.
    // Cleanup restores owner permissions on staged directories as necessary.
    match (result, remove_staging(&staging)) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => Err(crate::tr_format!(
            "{error}. Осталась временная копия «{}»: {cleanup}",
            "{error}. Temporary copy remains at “{}”: {cleanup}",
            staging.display()
        )),
        (Ok(()), Err(cleanup)) => Err(crate::tr_format!(
            "Копия «{}» готова, но не удалось удалить её пустую временную папку «{}»: {cleanup}",
            "Copy “{}” is ready, but its empty temporary folder “{}” could not be removed: {cleanup}",
            destination.display(),
            staging.display()
        )),
    }
}

#[cfg(unix)]
pub(crate) fn available_copy_space(parent: &Path) -> Result<u128, String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(parent.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(space_query_error(parent, io::Error::last_os_error()));
    }
    let stat = unsafe { stat.assume_init() };
    Ok((stat.f_bavail as u128) * (stat.f_frsize as u128))
}

#[cfg(windows)]
pub(crate) fn available_copy_space(parent: &Path) -> Result<u128, String> {
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
        return Err("Недопустимый путь проверки свободного места".into());
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
        return Err(space_query_error(parent, io::Error::last_os_error()));
    }
    Ok(u128::from(available))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn available_copy_space(_parent: &Path) -> Result<u128, String> {
    Err("Проверка свободного места на этой платформе недоступна".into())
}

#[cfg(any(unix, windows))]
fn space_query_error(parent: &Path, error: io::Error) -> String {
    let parent = parent.display();
    crate::tr_format!(
        "Не удалось проверить свободное место в папке «{parent}»: {error}",
        "Could not check available space in “{parent}”: {error}"
    )
}

pub(crate) fn copy_space_error(parent: &Path, error: &io::Error) -> Option<String> {
    match error.kind() {
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => {
            let parent = parent.display();
            Some(crate::tr_format!(
                "Недостаточно свободного места при копировании в папку «{parent}»: {error}",
                "Not enough free space while copying into “{parent}”: {error}"
            ))
        }
        _ => None,
    }
}

pub(crate) fn ensure_copy_space(
    required: u64,
    available: u128,
    parent: impl std::fmt::Display,
) -> Result<(), String> {
    if u128::from(required) > available {
        Err(crate::tr_format!(
            "Недостаточно свободного места в папке «{parent}»: требуется примерно {required} байт, доступно {available} байт",
            "Not enough free space in “{parent}”: about {required} bytes needed, {available} bytes available"
        ))
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
    io::Error::other(crate::i18n::source_changed_during_copy(path.display()))
}

fn directory_names(path: &Path) -> io::Result<Vec<OsString>> {
    let mut names = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
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
        let matches = source_metadata_matches(&self.metadata, actual)?;
        if matches {
            Ok(())
        } else {
            Err(source_changed(path))
        }
    }

    fn verify(&self, path: &Path) -> io::Result<()> {
        let actual = fs::symlink_metadata(path)
            .map_err(|e| io::Error::other(format!("{}: {e}", source_changed(path))))?;
        self.verify_metadata(path, &actual)?;
        if let Some(names) = &self.children {
            if *names != directory_names(path)? {
                return Err(source_changed(path));
            }
        }
        if let Some(link) = &self.link {
            if *link != fs::read_link(path)? {
                return Err(source_changed(path));
            }
        }
        Ok(())
    }
}

impl SourceManifest {
    fn capture(path: &Path) -> io::Result<Self> {
        let mut manifest = Self {
            entries: BTreeMap::new(),
            bytes: 0,
        };
        manifest.capture_entry(path, 0)?;
        Ok(manifest)
    }

    fn capture_entry(&mut self, path: &Path, depth: usize) -> io::Result<()> {
        if depth > 128 {
            return Err(io::Error::other("слишком большая глубина папок"));
        }
        let metadata = fs::symlink_metadata(path)?;
        // Do not silently claim a metadata check when the filesystem cannot
        // supply the modification time.
        metadata.modified()?;
        let children = if metadata.is_dir() {
            Some(directory_names(path)?)
        } else {
            None
        };
        let link = if metadata.is_symlink() {
            Some(fs::read_link(path)?)
        } else {
            None
        };
        if metadata.is_file() {
            self.bytes = self
                .bytes
                .checked_add(metadata.len())
                .ok_or_else(|| io::Error::other("слишком большой размер"))?;
        } else if !metadata.is_dir() && !metadata.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "специальный файл",
            ));
        }
        if let Some(names) = &children {
            for name in names {
                self.capture_entry(&path.join(name), depth + 1)?;
            }
        }
        let entry = SourceEntry {
            metadata,
            children,
            link,
        };
        entry.verify(path)?;
        self.entries.insert(path.to_path_buf(), entry);
        Ok(())
    }

    fn verify(&self) -> io::Result<()> {
        for (path, entry) in &self.entries {
            entry.verify(path)?;
        }
        Ok(())
    }
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    progress: &mut Progress,
    manifest: &SourceManifest,
    after_entry: &mut impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    progress.item(source);
    let entry = manifest
        .entries
        .get(source)
        .ok_or_else(|| source_changed(source))?;
    entry.verify(source)?;
    let metadata = &entry.metadata;
    if metadata.file_type().is_symlink() {
        copy_symlink(source, destination, metadata)?;
    } else if metadata.is_dir() {
        fs::create_dir(destination)?;
        for name in entry.children.as_ref().expect("directory manifest") {
            copy_tree(
                &source.join(name),
                &destination.join(name),
                progress,
                manifest,
                after_entry,
            )?;
        }
        fs::set_permissions(destination, metadata.permissions())?;
    } else if metadata.is_file() {
        let mut input = open_regular_source(source)?;
        entry.verify_metadata(source, &input.metadata()?)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)?;
        let written = progress.copy_local(source, metadata.len(), &mut input, &mut output)?;
        if written != metadata.len() {
            return Err(source_changed(source));
        }
        entry.verify_metadata(source, &input.metadata()?)?;
        entry.verify(source)?;
        output.sync_all()?;
        if let Ok(modified) = metadata.modified() {
            output.set_times(fs::FileTimes::new().set_modified(modified))?;
        }
        output.set_permissions(metadata.permissions())?;
    } else {
        return Err(io::Error::new(io::ErrorKind::Unsupported, format!("«{}» — специальный файл (сокет, устройство или канал). Можно копировать обычные файлы, папки и символические ссылки", source.display())));
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
    let input = options.open(source)?;
    let metadata = input.metadata()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(source_changed(source));
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
fn copy_symlink(source: &Path, destination: &Path, _: &fs::Metadata) -> io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(source)?, destination)
}

#[cfg(windows)]
fn copy_symlink(source: &Path, destination: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::windows::fs::FileTypeExt;
    let target = fs::read_link(source)?;
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
        "Копирование символических ссылок на этой платформе не поддерживается",
    ))
}

/// Restore permissions only inside an unpublished private copy before cleanup;
/// never follow a symlink or change any source permissions.
fn remove_staging(path: &Path) -> io::Result<()> {
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
    make_removable(path)?;
    fs::remove_dir_all(path)
}

fn at(action: &str, path: &Path, error: io::Error) -> String {
    format!("{action} «{}»: {error}", path.display())
}

pub(crate) fn rename_error(source: &Path, destination: &Path, error: io::Error) -> String {
    #[cfg(unix)]
    let cross_volume = error.raw_os_error() == Some(18); // EXDEV
    #[cfg(windows)]
    let cross_volume = error.raw_os_error() == Some(17); // ERROR_NOT_SAME_DEVICE
    #[cfg(not(any(unix, windows)))]
    let cross_volume = false;
    if cross_volume {
        return format!("Перемещение между дисками пока не поддерживается: {error}. Скопируйте элемент, проверьте копию, затем переместите оригинал в корзину");
    }
    format!(
        "Не удалось переместить «{}» в «{}» без замены существующих элементов: {error}",
        source.display(),
        destination.display()
    )
}

// Native no-replacement renames close the check-then-rename overwrite race.
// If a platform/filesystem does not support them, fail without a risky fallback.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Путь содержит нулевой символ"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Путь содержит нулевой символ"))?;
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
            "Путь содержит нулевой символ",
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
        "Безопасное переименование без перезаписи на этой платформе пока не реализовано",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_file_changes_abort_before_publication_and_clean_staging() {
        for mutation in ["append", "truncate", "rewrite", "remove", "directory"] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("source");
            let destination = root.path().join("destination");
            fs::write(&source, b"original").unwrap();
            fs::write(&destination, b"previous target").unwrap();
            let mut published = false;
            let result = copy_atomically_checked(
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
            assert!(
                error.contains("changed during copying"),
                "{mutation}: {error}"
            );
            assert!(!published, "{mutation}");
            assert_eq!(fs::read(&destination).unwrap(), b"previous target");
            assert!(!fs::read_dir(root.path()).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".pawcommander-copy-")));
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
        assert!(manifest.verify().is_err());
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
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::write(&source, b"new contents").unwrap();
        fs::write(&destination, b"existing contents").unwrap();
        ensure_copy_space(
            SourceManifest::capture(&source).unwrap().bytes,
            available_copy_space(root.path()).unwrap(),
            root.path().display(),
        )
        .unwrap();
        let folder = root.path().display();
        let error = ensure_copy_space(12, 11, &folder).unwrap_err();
        assert!(
            error.contains(root.path().to_string_lossy().as_ref()),
            "{error}"
        );
        assert!(ensure_copy_space(12, 12, &folder).is_ok());
        assert!(ensure_copy_space(u64::MAX, u128::from(u64::MAX) + 1, folder).is_ok());
        assert!(available_copy_space(&root.path().join("missing")).is_err());
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
        assert!(ensure_copy_space(bytes, 8, root.path().display()).is_err());
        assert!(ensure_copy_space(bytes, 9, root.path().display()).is_ok());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&source, source.join("link")).unwrap();
            assert_eq!(SourceManifest::capture(&source).unwrap().bytes, 9);
        }
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
        let source = root.path().join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        for kind in [TransferKind::Copy, TransferKind::Move] {
            assert!(transfer(std::slice::from_ref(&source), &source.join("nested"), kind).is_err());
            assert!(transfer(std::slice::from_ref(&source), &source, kind).is_err());
        }
        assert!(source.is_dir());
        assert_eq!(fs::read_dir(source.join("nested")).unwrap().count(), 0);
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

    #[cfg(unix)]
    #[test]
    fn regular_source_open_refuses_a_replacement_symlink() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let actual = root.path().join("actual");
        let replacement = root.path().join("replacement");
        fs::write(&actual, b"private").unwrap();
        symlink(&actual, &replacement).unwrap();
        assert!(open_regular_source(&replacement).is_err());
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
            assert!(open_regular_source(Path::new(&fifo)).is_err());
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
