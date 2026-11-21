//! Durable fingerprints of the last verified common tree, scoped to the folder pair.
use super::super::{MAX_ENTRIES, MAX_PATH_BYTES, MAX_TREE_DEPTH};
use super::{Kind, Snapshot, Tree};
use crate::{
    i18n::text,
    operations::{Cancellation, TransferError},
    recovery::{same_owned_marker, sync_directory},
    vfs::{err, sftp, transfer::EntryMetadata, Location},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, BufWriter, Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};

const VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
enum RootId {
    Local {
        path: String,
        device: u64,
        inode: u64,
        created: Option<(u64, u32)>,
    },
    Sftp {
        path: String,
        host: String,
        port: u16,
        user: String,
        server_key: String,
    },
}

impl RootId {
    fn capture(snapshot: &Snapshot) -> Result<Self, TransferError> {
        let path = URL_SAFE_NO_PAD.encode(snapshot.root.path().as_os_str().as_bytes());
        match &snapshot.root {
            Location::Local(_) => {
                let root = snapshot.entries.get(Path::new("")).ok_or_else(invalid)?;
                let EntryMetadata::Local(metadata) = &root.version.metadata else {
                    return Err(invalid());
                };
                let created = metadata
                    .created()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|time| (time.as_secs(), time.subsec_nanos()));
                Ok(Self::Local {
                    path,
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    created,
                })
            }
            Location::Sftp { endpoint, .. } => {
                let session = sftp::sessions()
                    .lock()
                    .map_err(err)?
                    .get(endpoint)
                    .cloned()
                    .ok_or_else(|| text("Не удалось подтвердить сервер для общего состояния"))?;
                let (key, _) = session
                    .host_key()
                    .ok_or_else(|| text("Не удалось подтвердить сервер для общего состояния"))?;
                Ok(Self::Sftp {
                    path,
                    host: endpoint.host_name().to_owned(),
                    port: endpoint.port,
                    user: endpoint.user.clone(),
                    server_key: URL_SAFE_NO_PAD.encode(Sha256::digest(key)),
                })
            }
            Location::Archive { .. } => Err(invalid()),
        }
    }
}

fn capture_pair(observed: &[Snapshot; 2]) -> Result<([RootId; 2], [usize; 2]), TransferError> {
    let mut pair = [
        RootId::capture(&observed[0])?,
        RootId::capture(&observed[1])?,
    ];
    let mut panes = [0, 1];
    if pair[0] > pair[1] {
        pair.swap(0, 1);
        panes.swap(0, 1);
    }
    Ok((pair, panes))
}

pub(super) struct BaselineStore {
    pub(super) directory: PathBuf,
    pub(super) path: PathBuf,
    pair: [RootId; 2],
    pub(super) panes: [usize; 2],
    exclusions: Vec<String>,
}

pub(super) struct PairLock<'a> {
    pub(super) store: &'a BaselineStore,
    _lock: File,
}

#[derive(Serialize)]
struct WriteRecord<'a> {
    version: u32,
    pair: &'a [RootId; 2],
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    exclusions: &'a [String],
    entries: Vec<(String, DiskKind)>,
}

#[derive(Deserialize)]
struct ReadRecord {
    version: u32,
    pair: [RootId; 2],
    #[serde(default)]
    exclusions: Vec<String>,
    entries: Vec<(String, DiskKind)>,
}

#[derive(Serialize, Deserialize)]
enum DiskKind {
    Directory,
    File(String),
    Link(String),
}

impl BaselineStore {
    pub(super) fn new(
        directory: PathBuf,
        observed: &[Snapshot; 2],
        exclusions: &[String],
    ) -> Result<Self, TransferError> {
        let (pair, panes) = capture_pair(observed)?;
        check_storage_location(&directory, &pair)?;
        let seed = serde_json::to_vec(&pair).map_err(err)?;
        let filename = URL_SAFE_NO_PAD.encode(Sha256::digest(seed));
        let path = directory.join(format!("pair-{filename}.json"));
        Ok(Self {
            directory,
            path,
            pair,
            panes,
            exclusions: exclusions.to_vec(),
        })
    }

    pub(super) fn check_roots(&self, observed: &[Snapshot; 2]) -> Result<(), TransferError> {
        let (pair, panes) = capture_pair(observed)?;
        if pair != self.pair || panes != self.panes {
            return Err(text("Пара папок изменилась. Постройте план заново.").into());
        }
        Ok(())
    }

    pub(super) fn read(&self, cancel: &Cancellation) -> Result<Option<Tree>, TransferError> {
        cancel.check()?;
        if !self.directory_exists()? {
            return Ok(None);
        }
        let Some(record): Option<ReadRecord> = read_record(&self.path, MAX_FILE_BYTES)? else {
            return Ok(None);
        };
        if record.version != VERSION
            || record.pair != self.pair
            || record.entries.len() > MAX_ENTRIES
        {
            return Err(invalid());
        }
        let mut tree = Tree::new();
        let mut bytes = 0;
        for (path, kind) in record.entries {
            cancel.check()?;
            let path = decode_path(&path)?;
            let kind = match kind {
                DiskKind::Directory => Kind::Directory,
                DiskKind::File(digest) => {
                    Kind::File(decode(&digest)?.try_into().map_err(|_| invalid())?)
                }
                DiskKind::Link(target) => {
                    let target = decode(&target)?;
                    if target.is_empty() || target.contains(&0) {
                        return Err(invalid());
                    }
                    bytes += target.len();
                    Kind::Link(OsString::from_vec(target))
                }
            };
            bytes += path.as_os_str().len();
            if bytes > MAX_PATH_BYTES || tree.insert(path, kind).is_some() {
                return Err(invalid());
            }
        }
        for path in tree.keys() {
            cancel.check()?;
            let parent = path.parent().ok_or_else(invalid)?;
            if !parent.as_os_str().is_empty() && !matches!(tree.get(parent), Some(Kind::Directory))
            {
                return Err(invalid());
            }
        }
        if record.exclusions != self.exclusions {
            return Ok(None);
        }
        Ok(Some(tree))
    }

    pub(super) fn lock(&self) -> Result<PairLock<'_>, TransferError> {
        if !self.directory_exists()? {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.directory)?;
            if !self.directory_exists()? {
                return Err(invalid());
            }
        }
        let path = self.path.with_extension("lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        match lock.try_lock() {
            Ok(()) if same_owned_marker(&lock, &path)? => Ok(PairLock {
                store: self,
                _lock: lock,
            }),
            Ok(()) => Err(invalid()),
            Err(TryLockError::WouldBlock) => {
                Err(text("Синхронизация этой пары уже выполняется.").into())
            }
            Err(error) => Err(err(error).into()),
        }
    }

    pub(super) fn directory_exists(&self) -> Result<bool, TransferError> {
        match fs::symlink_metadata(&self.directory) {
            Ok(metadata) if metadata.is_dir() => Ok(true),
            Ok(_) => Err(invalid()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

impl PairLock<'_> {
    pub(super) fn save(&self, tree: &Tree, cancel: &Cancellation) -> Result<(), TransferError> {
        cancel.check()?;
        let mut entries = Vec::with_capacity(tree.len());
        for (path, kind) in tree {
            cancel.check()?;
            let path = URL_SAFE_NO_PAD.encode(path.as_os_str().as_bytes());
            let kind = match kind {
                Kind::Directory => DiskKind::Directory,
                Kind::File(digest) => DiskKind::File(URL_SAFE_NO_PAD.encode(digest)),
                Kind::Link(target) => DiskKind::Link(URL_SAFE_NO_PAD.encode(target.as_bytes())),
            };
            entries.push((path, kind));
        }
        self.write(
            &self.store.path,
            &WriteRecord {
                version: VERSION,
                pair: &self.store.pair,
                exclusions: &self.store.exclusions,
                entries,
            },
            MAX_FILE_BYTES,
            cancel,
        )
    }

    pub(super) fn write(
        &self,
        path: &Path,
        record: &impl Serialize,
        limit: u64,
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        let mut temporary = tempfile::NamedTempFile::new_in(&self.store.directory)?;
        let mut writer = BufWriter::new(temporary.as_file_mut());
        serde_json::to_writer(&mut writer, record).map_err(err)?;
        writer.flush()?;
        drop(writer);
        if temporary.as_file().metadata()?.len() > limit {
            return Err(invalid());
        }
        temporary.as_file().sync_all()?;
        cancel.check()?;
        temporary.persist(path).map_err(|error| error.error)?;
        sync_directory(&self.store.directory)?;
        Ok(())
    }
}

pub(super) fn read_record<T: serde::de::DeserializeOwned>(
    path: &Path,
    limit: u64,
) -> Result<Option<T>, TransferError> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(invalid());
    }
    serde_json::from_reader(io::BufReader::new(file.take(limit)))
        .map(Some)
        .map_err(|_| invalid())
}

fn check_storage_location(directory: &Path, pair: &[RootId; 2]) -> Result<(), TransferError> {
    let mut resolved = None;
    for candidate in directory.ancestors() {
        match candidate.canonicalize() {
            Ok(path) => {
                resolved = Some(path);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let resolved = resolved.ok_or_else(invalid)?;
    for parent in resolved.ancestors() {
        let metadata = parent.metadata()?;
        if pair.iter().any(|root| {
            matches!(root, RootId::Local { device, inode, .. }
                if (*device, *inode) == (metadata.dev(), metadata.ino()))
        }) {
            return Err(text("Одна из папок содержит данные синхронизации приложения.").into());
        }
    }
    Ok(())
}

fn decode(value: &str) -> Result<Vec<u8>, TransferError> {
    URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())
}

pub(super) fn decode_path(value: &str) -> Result<PathBuf, TransferError> {
    let bytes = decode(value)?;
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(invalid());
    }
    let path = PathBuf::from(OsString::from_vec(bytes));
    let parts = path.components();
    if parts.clone().count() > MAX_TREE_DEPTH + 1
        || parts
            .clone()
            .any(|part| !matches!(part, Component::Normal(_)))
        || parts.collect::<PathBuf>().as_os_str() != path.as_os_str()
    {
        return Err(invalid());
    }
    Ok(path)
}

pub(super) fn invalid() -> TransferError {
    text("Сохранённое общее состояние повреждено или имеет неподдерживаемый формат.").into()
}
