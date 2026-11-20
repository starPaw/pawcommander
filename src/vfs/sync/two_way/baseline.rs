//! Durable fingerprints of the last verified common tree, scoped to the folder pair.
use super::super::{MAX_ENTRIES, MAX_PATH_BYTES, MAX_TREE_DEPTH};
use super::{Kind, Snapshot, Tree};
use crate::{
    i18n::text,
    operations::{Cancellation, TransferError},
    vfs::{err, sftp, transfer::EntryMetadata, Location},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
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

fn capture_pair(observed: &[Snapshot; 2]) -> Result<[RootId; 2], TransferError> {
    let mut pair = [
        RootId::capture(&observed[0])?,
        RootId::capture(&observed[1])?,
    ];
    pair.sort();
    Ok(pair)
}

pub(super) struct BaselineStore {
    directory: PathBuf,
    path: PathBuf,
    pair: [RootId; 2],
}

#[derive(Serialize)]
struct WriteRecord<'a> {
    version: u32,
    pair: &'a [RootId; 2],
    entries: Vec<(String, DiskKind)>,
}

#[derive(Deserialize)]
struct ReadRecord {
    version: u32,
    pair: [RootId; 2],
    entries: Vec<(String, DiskKind)>,
}

#[derive(Serialize, Deserialize)]
enum DiskKind {
    Directory,
    File(String),
    Link(String),
}

impl BaselineStore {
    pub(super) fn new(directory: PathBuf, observed: &[Snapshot; 2]) -> Result<Self, TransferError> {
        let pair = capture_pair(observed)?;
        check_storage_location(&directory, &pair)?;
        let seed = serde_json::to_vec(&pair).map_err(err)?;
        let filename = URL_SAFE_NO_PAD.encode(Sha256::digest(seed));
        let path = directory.join(format!("pair-{filename}.json"));
        Ok(Self {
            directory,
            path,
            pair,
        })
    }

    pub(super) fn check_roots(&self, observed: &[Snapshot; 2]) -> Result<(), TransferError> {
        if capture_pair(observed)? != self.pair {
            return Err(text("Пара папок изменилась. Постройте план заново.").into());
        }
        Ok(())
    }

    pub(super) fn read(&self, cancel: &Cancellation) -> Result<Option<Tree>, TransferError> {
        cancel.check()?;
        if !self.directory_exists()? {
            return Ok(None);
        }
        let open = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.path);
        let file = match open {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            return Err(invalid());
        }
        let record: ReadRecord =
            serde_json::from_reader(io::BufReader::new(file.take(MAX_FILE_BYTES)))
                .map_err(|_| invalid())?;
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
        Ok(Some(tree))
    }

    pub(super) fn seed(&self, tree: &Tree, cancel: &Cancellation) -> Result<(), TransferError> {
        cancel.check()?;
        if !self.directory_exists()? {
            fs::create_dir_all(&self.directory)?;
            if !self.directory_exists()? {
                return Err(invalid());
            }
        }
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
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        let mut writer = BufWriter::new(temporary.as_file_mut());
        serde_json::to_writer(
            &mut writer,
            &WriteRecord {
                version: VERSION,
                pair: &self.pair,
                entries,
            },
        )
        .map_err(err)?;
        writer.flush()?;
        drop(writer);
        if temporary.as_file().metadata()?.len() > MAX_FILE_BYTES {
            return Err(invalid());
        }
        temporary.as_file().sync_all()?;
        cancel.check()?;
        temporary.persist_noclobber(&self.path).map_err(|error| {
            if error.error.kind() == io::ErrorKind::AlreadyExists {
                text("Общее состояние изменилось. Постройте план заново.").into()
            } else {
                TransferError::from(error.error)
            }
        })?;
        crate::recovery::sync_directory(&self.directory)?;
        Ok(())
    }

    fn directory_exists(&self) -> Result<bool, TransferError> {
        match fs::symlink_metadata(&self.directory) {
            Ok(metadata) if metadata.is_dir() => Ok(true),
            Ok(_) => Err(invalid()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
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

fn decode_path(value: &str) -> Result<PathBuf, TransferError> {
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

fn invalid() -> TransferError {
    text("Сохранённое общее состояние повреждено или имеет неподдерживаемый формат.").into()
}
