//! Local/SFTP locations. Remote credentials live only in authenticated sessions;
//! saved tabs/bookmarks contain connection coordinates and key paths, never secrets.
use crate::conflict::{Answer, Conflict, Decision, EntryDetails};
use crate::fs_ops::{self, TransferKind};
use crate::operations::Progress;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use ssh2::{
    CheckResult, ErrorCode, FileStat, KnownHostFileKind, OpenFlags, OpenType, RenameFlags, Session,
    Sftp,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key: Option<PathBuf>,
}

// Untagged keeps persisted local paths compatible with the original settings.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Location {
    Local(PathBuf),
    Sftp { endpoint: Endpoint, path: PathBuf },
}
impl From<PathBuf> for Location {
    fn from(path: PathBuf) -> Self {
        Self::Local(path)
    }
}
impl Location {
    pub fn path(&self) -> &Path {
        match self {
            Self::Local(p) | Self::Sftp { path: p, .. } => p,
        }
    }
    pub fn local(&self) -> Option<&Path> {
        match self {
            Self::Local(p) => Some(p),
            _ => None,
        }
    }
    pub fn remote(&self) -> bool {
        matches!(self, Self::Sftp { .. })
    }
    pub fn with_path(&self, path: PathBuf) -> Self {
        match self {
            Self::Local(_) => Self::Local(path),
            Self::Sftp { endpoint, .. } => Self::Sftp {
                endpoint: endpoint.clone(),
                path,
            },
        }
    }
    pub fn join(&self, name: impl AsRef<Path>) -> Self {
        let name = name.as_ref();
        if !self.remote() {
            return self.with_path(self.path().join(name));
        }
        // Remote paths use POSIX separators even when the client runs on Windows.
        // Preserve the supplied name so validation can still reject backslashes.
        if name.as_os_str().as_encoded_bytes().starts_with(b"/") {
            return self.with_path(name.into());
        }
        let mut path = self.path().as_os_str().to_owned();
        if !name.as_os_str().is_empty() {
            if !path.as_encoded_bytes().ends_with(b"/") {
                path.push("/");
            }
            path.push(name);
        }
        self.with_path(path.into())
    }
    pub fn parent(&self) -> Option<Self> {
        self.path().parent().map(|p| self.with_path(p.into()))
    }
    pub fn ancestors(&self) -> impl Iterator<Item = Self> + '_ {
        self.path().ancestors().map(|p| self.with_path(p.into()))
    }
    pub fn display(&self) -> String {
        self.to_string()
    }
    pub fn name(&self) -> String {
        self.path()
            .file_name()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.to_string())
    }
    pub(crate) fn same_backend(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local(_), Self::Local(_)) => true,
            (Self::Sftp { endpoint: a, .. }, Self::Sftp { endpoint: b, .. }) => a == b,
            _ => false,
        }
    }
}
impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(p) => write!(f, "{}", p.display()),
            Self::Sftp { endpoint, path } => {
                let mut url = url::Url::parse("sftp://localhost/").map_err(|_| fmt::Error)?;
                let host = endpoint.host.trim_matches(['[', ']']);
                let host = if host.contains(':') {
                    format!("[{host}]")
                } else {
                    host.to_owned()
                };
                url.set_host(Some(&host)).map_err(|_| fmt::Error)?;
                let user = percent_encoding::utf8_percent_encode(
                    &endpoint.user,
                    percent_encoding::NON_ALPHANUMERIC,
                )
                .to_string();
                url.set_username(&user).map_err(|_| fmt::Error)?;
                if endpoint.port != 22 {
                    url.set_port(Some(endpoint.port)).map_err(|_| fmt::Error)?;
                }
                let path = path.to_string_lossy().replace('\\', "/");
                url.path_segments_mut()
                    .map_err(|_| fmt::Error)?
                    .clear()
                    .extend(path.split('/').skip(1));
                write!(f, "{url}")
            }
        }
    }
}

pub fn resolve(address: &str, current: &Location) -> Result<Location, String> {
    if address.starts_with("sftp:") {
        let url = url::Url::parse(address).map_err(|e| {
            crate::tr_format!("Некорректный адрес SFTP: {e}", "Invalid SFTP address: {e}")
        })?;
        if url.scheme() != "sftp"
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(crate::i18n::text(
                "Используйте sftp://пользователь@сервер:22/путь без пароля, query и fragment",
            )
            .into());
        }
        let host = url
            .host_str()
            .ok_or(crate::i18n::text("Укажите сервер SFTP"))?
            .to_owned();
        let user = percent_decode_str(url.username())
            .decode_utf8()
            .map_err(|_| crate::i18n::text("Имя пользователя должно быть UTF-8"))?
            .into_owned();
        if user.is_empty() {
            return Err(
                crate::i18n::text("Укажите пользователя: sftp://пользователь@сервер/путь").into(),
            );
        }
        let mut endpoint = Endpoint {
            host,
            port: url.port().unwrap_or(22),
            user,
            key: None,
        };
        if let Location::Sftp {
            endpoint: existing, ..
        } = current
        {
            if endpoint.host == existing.host
                && endpoint.port == existing.port
                && endpoint.user == existing.user
            {
                endpoint.key = existing.key.clone();
            }
        }
        let path = percent_decode_str(url.path())
            .decode_utf8()
            .map_err(|_| crate::i18n::text("Путь должен быть UTF-8"))?;
        if path.contains('\0') {
            return Err(crate::i18n::text("Недопустимый путь").into());
        }
        return Ok(Location::Sftp {
            endpoint,
            path: normalize_remote(Path::new(if path.is_empty() { "/" } else { &path }))?,
        });
    }
    if current.remote() {
        if address == "~" || address.starts_with("~/") {
            return Err(crate::i18n::text(
                "Для домашней папки сервера используйте кнопку SFTP и начальный путь «.»",
            )
            .into());
        }
        let path = if address.starts_with('/') {
            PathBuf::from(address)
        } else {
            current.join(address).path().to_owned()
        };
        return Ok(current.with_path(normalize_remote(&path)?));
    }
    let path = if address == "~" {
        dirs::home_dir().unwrap_or_else(|| current.path().into())
    } else if let Some(rest) = address.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| current.path().into())
            .join(rest)
    } else {
        PathBuf::from(address)
    };
    Ok(if path.is_absolute() {
        Location::Local(path)
    } else {
        current.join(path)
    })
}
fn normalize_remote(path: &Path) -> Result<PathBuf, String> {
    let text = path
        .to_str()
        .ok_or(crate::i18n::text("Путь SFTP должен быть UTF-8"))?;
    if !text.starts_with('/') || text.contains(['\0', '\\']) {
        return Err(crate::i18n::text(
            "Используйте абсолютный POSIX-путь SFTP без обратных слешей",
        )
        .into());
    }
    let mut parts = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    Ok(PathBuf::from(format!("/{}", parts.join("/"))))
}

static SESSIONS: OnceLock<Mutex<HashMap<Endpoint, Session>>> = OnceLock::new();
fn sessions() -> &'static Mutex<HashMap<Endpoint, Session>> {
    SESSIONS.get_or_init(Default::default)
}

#[cfg(test)]
pub fn connect(
    endpoint: &Endpoint,
    password: &str,
    passphrase: &str,
    known_hosts: &Path,
) -> Result<Session, String> {
    connect_with_key(endpoint, password, passphrase, known_hosts, None)
}
fn connect_with_key(
    endpoint: &Endpoint,
    password: &str,
    passphrase: &str,
    known_hosts: &Path,
    private_key: Option<&str>,
) -> Result<Session, String> {
    if endpoint.host.trim().is_empty() || endpoint.user.trim().is_empty() || endpoint.port == 0 {
        return Err("Укажите сервер, пользователя и порт 1–65535".into());
    }
    // Bound TCP and SSH I/O waits. DNS resolution uses the OS resolver.
    let host = endpoint.host.trim_matches(['[', ']']);
    let addresses = (host, endpoint.port)
        .to_socket_addrs()
        .map_err(|e| format!("Не удалось найти сервер: {e}"))?;
    let mut tcp = None;
    for address in addresses {
        if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_secs(10)) {
            tcp = Some(stream);
            break;
        }
    }
    let tcp = tcp.ok_or("Не удалось подключиться к серверу SFTP (тайм-аут 10 с на адрес)")?;
    tcp.set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(err)?;
    tcp.set_write_timeout(Some(Duration::from_secs(20)))
        .map_err(err)?;
    let mut session = Session::new().map_err(err)?;
    session.set_tcp_stream(tcp);
    session.set_timeout(20_000);
    session.handshake().map_err(err)?;
    let mut known = session.known_hosts().map_err(err)?;
    known.read_file(known_hosts, KnownHostFileKind::OpenSSH)
        .map_err(|e| format!("Не удалось прочитать {}: {e}. Сначала подключитесь к серверу через OpenSSH и проверьте его ключ.", known_hosts.display()))?;
    let (key, _) = session.host_key().ok_or("Сервер не предоставил ключ SSH")?;
    match known.check_port(host, endpoint.port, key) {
        CheckResult::Match => {},
        CheckResult::Mismatch => return Err("Ключ сервера SSH изменился. Подключение отклонено; проверьте ключ сервера в known_hosts.".into()),
        _ => return Err("Ключ сервера SSH не подтверждён. Сначала подключитесь через OpenSSH, проверьте fingerprint и сохраните ключ в ~/.ssh/known_hosts.".into()),
    }
    if !password.is_empty() {
        session
            .userauth_password(&endpoint.user, password)
            .map_err(err)?;
    } else if let Some(key) = private_key {
        session
            .userauth_pubkey_memory(
                &endpoint.user,
                None,
                key,
                (!passphrase.is_empty()).then_some(passphrase),
            )
            .map_err(err)?;
    } else if let Some(key) = &endpoint.key {
        session
            .userauth_pubkey_file(
                &endpoint.user,
                None,
                key,
                (!passphrase.is_empty()).then_some(passphrase),
            )
            .map_err(err)?;
    } else {
        session.userauth_agent(&endpoint.user).map_err(|e| {
            format!("SSH-agent: {e}. Добавьте ключ в agent или укажите ключ/пароль в диалоге SFTP.")
        })?;
    }
    if !session.authenticated() {
        return Err("Сервер не подтвердил аутентификацию".into());
    }
    Ok(session)
}
pub fn connect_location(
    endpoint: Endpoint,
    path: &str,
    password: String,
    passphrase: String,
    use_saved: bool,
    save_key: bool,
) -> Result<Location, String> {
    let known_hosts = dirs::home_dir()
        .ok_or("Не найдена домашняя папка")?
        .join(".ssh/known_hosts");
    let saved = if use_saved && !save_key && password.is_empty() && endpoint.key.is_some() {
        crate::credentials::load(&endpoint)?
    } else {
        None
    };
    let imported = if save_key {
        if !password.is_empty() {
            return Err("Для сохранения ключа очистите пароль аккаунта".into());
        }
        let key_path = endpoint
            .key
            .as_ref()
            .ok_or("Укажите файл приватного ключа")?;
        let mut bytes = Vec::new();
        fs::File::open(key_path)
            .map_err(err)?
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(err)?;
        if bytes.len() > 64 * 1024 {
            return Err("Ключ больше 64 КиБ".into());
        }
        Some(
            String::from_utf8(bytes)
                .map_err(|_| "Ключ должен быть в текстовом формате OpenSSH/PEM")?,
        )
    } else {
        None
    };
    let key_data = imported
        .as_deref()
        .or_else(|| saved.as_ref().map(|s| s.private_key.as_str()));
    let phrase = if !passphrase.is_empty() {
        passphrase.as_str()
    } else {
        saved.as_ref().map(|s| s.passphrase.as_str()).unwrap_or("")
    };
    let session = connect_with_key(&endpoint, &password, phrase, &known_hosts, key_data)?;
    let sftp = session.sftp().map_err(err)?;
    let path = sftp
        .realpath(Path::new(if path.is_empty() { "." } else { path }))
        .map_err(err)?;
    if !sftp.stat(&path).map_err(err)?.is_dir() {
        return Err("Начальный путь не является папкой".into());
    }
    if save_key {
        crate::credentials::save(
            &endpoint,
            &crate::credentials::SavedKey {
                private_key: imported.ok_or("Не прочитан ключ")?,
                passphrase: phrase.into(),
            },
        )?;
    }
    sessions()
        .lock()
        .map_err(err)?
        .insert(endpoint.clone(), session);
    Ok(Location::Sftp { endpoint, path })
}
fn remote(location: &Location) -> Result<Sftp, String> {
    let Location::Sftp { endpoint, .. } = location else {
        return Err("Ожидалось подключение SFTP".into());
    };
    let existing = sessions().lock().map_err(err)?.get(endpoint).cloned();
    let session = match existing {
        Some(session) => session,
        None => {
            let known_hosts = dirs::home_dir()
                .ok_or("Не найдена домашняя папка")?
                .join(".ssh/known_hosts");
            let saved = if endpoint.key.is_some() {
                crate::credentials::load(endpoint)?
            } else {
                None
            };
            let session = connect_with_key(
                endpoint,
                "",
                saved.as_ref().map(|s| s.passphrase.as_str()).unwrap_or(""),
                &known_hosts,
                saved.as_ref().map(|s| s.private_key.as_str()),
            )?;
            sessions()
                .lock()
                .map_err(err)?
                .insert(endpoint.clone(), session.clone());
            session
        }
    };
    session
        .sftp()
        .map_err(|e| format!("SFTP: {e}. Для повторного подключения используйте кнопку SFTP."))
}
fn err(error: impl fmt::Display) -> String {
    error.to_string()
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: Location,
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}
pub fn read_directory(location: &Location) -> Result<Vec<FileEntry>, String> {
    if let Some(path) = location.local() {
        return fs_ops::read_directory(path).map(|entries| {
            entries
                .into_iter()
                .map(|e| FileEntry {
                    path: e.path.into(),
                    name: e.name,
                    is_dir: e.is_dir,
                    is_symlink: e.is_symlink,
                    size: e.size,
                    modified: e.modified,
                })
                .collect()
        });
    }
    read_sftp_directory(location, &remote(location)?)
}

/// Completion may browse an existing session without initiating authentication.
pub fn read_address_directory(location: &Location) -> Result<Vec<FileEntry>, String> {
    let Location::Sftp { endpoint, .. } = location else {
        return read_directory(location);
    };
    let session = sessions().lock().map_err(err)?.get(endpoint).cloned();
    match session {
        Some(session) => read_sftp_directory(location, &session.sftp().map_err(err)?),
        None => Ok(vec![]),
    }
}

fn read_sftp_directory(location: &Location, sftp: &Sftp) -> Result<Vec<FileEntry>, String> {
    let mut entries = Vec::new();
    for (path, stat) in sftp.readdir(location.path()).map_err(err)? {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("Имя файла SFTP не является UTF-8")?;
        validate_name(name)?;
        if path != location.path().join(name) {
            return Err("Сервер вернул имя, несовместимое с путями этой ОС".into());
        }
        let is_symlink = stat.file_type().is_symlink();
        let is_dir =
            stat.is_dir() || (is_symlink && sftp.stat(&path).map(|s| s.is_dir()).unwrap_or(false));
        entries.push(FileEntry {
            path: location.join(name),
            name: name.into(),
            is_dir,
            is_symlink,
            size: stat.size.unwrap_or(0),
            modified: stat
                .mtime
                .and_then(|s| UNIX_EPOCH.checked_add(Duration::from_secs(s))),
        });
    }
    Ok(entries)
}
fn validate_name(name: &str) -> Result<(), String> {
    let mut components = Path::new(name).components();
    if name.trim().is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', '\0'])
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err("Введите имя без /, \\, NUL, . и ..".into());
    }
    Ok(())
}
pub fn create_directory(parent: &Location, name: &str) -> Result<Location, String> {
    validate_name(name)?;
    if let Some(path) = parent.local() {
        return fs_ops::create_directory(path, name).map(Location::Local);
    }
    let target = parent.join(name);
    remote(parent)?.mkdir(target.path(), 0o755).map_err(err)?;
    Ok(target)
}
pub fn rename_entry(source: &Location, name: &str) -> Result<Location, String> {
    validate_name(name)?;
    if let Some(path) = source.local() {
        return fs_ops::rename_entry(path, name).map(Location::Local);
    }
    let target = source
        .parent()
        .ok_or("Нельзя переименовать корень")?
        .join(name);
    if &target != source {
        remote(source)?
            .rename(source.path(), target.path(), Some(RenameFlags::empty()))
            .map_err(err)?;
    }
    Ok(target)
}
enum EntryMetadata {
    Local(fs::Metadata),
    Remote(FileStat),
}

impl EntryMetadata {
    fn is_file(&self) -> bool {
        match self {
            Self::Local(meta) => meta.is_file(),
            Self::Remote(stat) => stat.is_file(),
        }
    }

    fn same_object(&self, other: &Self) -> bool {
        #[cfg(unix)]
        if let (Self::Local(left), Self::Local(right)) = (self, other) {
            use std::os::unix::fs::MetadataExt;
            return left.dev() == right.dev() && left.ino() == right.ino();
        }
        let _ = other;
        false
    }

    // Access time can change just by opening a file. Compare the fields
    // relevant to replacement, including the local inode where available.
    fn matches_file(&self, current: &Self) -> bool {
        match (self, current) {
            (Self::Local(expected), Self::Local(actual)) => {
                #[cfg(unix)]
                if !self.same_object(current) {
                    return false;
                }
                actual.is_file()
                    && expected.len() == actual.len()
                    && expected.modified().ok() == actual.modified().ok()
                    && expected.created().ok() == actual.created().ok()
                    && expected.permissions() == actual.permissions()
            }
            (Self::Remote(expected), Self::Remote(actual)) => {
                actual.is_file()
                    && expected.size == actual.size
                    && expected.mtime == actual.mtime
                    && expected.perm == actual.perm
                    && expected.uid == actual.uid
                    && expected.gid == actual.gid
            }
            _ => false,
        }
    }

    fn details(&self, location: &Location) -> EntryDetails {
        let (kind, size, modified) = match self {
            Self::Local(meta) => (
                entry_kind(meta.is_file(), meta.is_dir(), meta.is_symlink()),
                (!meta.is_dir()).then_some(meta.len()),
                meta.modified().ok(),
            ),
            Self::Remote(stat) => (
                entry_kind(stat.is_file(), stat.is_dir(), stat.file_type().is_symlink()),
                stat.size.filter(|_| !stat.is_dir()),
                stat.mtime
                    .and_then(|time| UNIX_EPOCH.checked_add(Duration::from_secs(time))),
            ),
        };
        EntryDetails {
            location: location.clone(),
            kind,
            size,
            modified,
        }
    }
}

fn entry_kind(file: bool, dir: bool, symlink: bool) -> &'static str {
    if file {
        "Файл"
    } else if dir {
        "Папка"
    } else if symlink {
        "Ссылка"
    } else {
        "Специальный объект"
    }
}

fn entry_metadata(location: &Location) -> Result<Option<EntryMetadata>, String> {
    if let Some(path) = location.local() {
        match fs::symlink_metadata(path) {
            Ok(meta) => Ok(Some(EntryMetadata::Local(meta))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{location}: {e}")),
        }
    } else {
        match remote(location)?.lstat(location.path()) {
            Ok(stat) => Ok(Some(EntryMetadata::Remote(stat))),
            Err(e) if e.code() == ErrorCode::SFTP(2) => Ok(None),
            Err(e) => Err(format!("{location}: {e}")),
        }
    }
}

struct TransferItem {
    original: Location,
    source: Location,
    target: Location,
    replacement: Option<(EntryMetadata, EntryMetadata)>,
}

impl TransferItem {
    fn new(original: Location, source: Location, target: Location) -> Self {
        Self {
            original,
            source,
            target,
            replacement: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct TransferSummary {
    pub completed: usize,
    pub skipped: usize,
    pub cancelled: bool,
}

fn transfer_plan(
    sources: &[Location],
    destination: &Location,
    kind: TransferKind,
) -> Result<Vec<TransferItem>, String> {
    if let Some(dest) = destination.local() {
        if sources.iter().all(|p| !p.remote()) {
            let paths = sources.iter().map(|p| p.path().into()).collect::<Vec<_>>();
            return fs_ops::plan_transfer(&paths, dest).map(|plan| {
                plan.into_iter()
                    .map(|(original, source, target)| {
                        TransferItem::new(original.into(), source.into(), target.into())
                    })
                    .collect()
            });
        }
    }

    let mut names = HashSet::new();
    let mut plan = Vec::new();
    for source in sources {
        let name = source
            .path()
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("Нельзя переносить корень или имя не UTF-8")?;
        validate_name(name)?;
        if !names.insert(name.to_owned()) {
            return Err("Выбраны элементы с одинаковыми именами".into());
        }
        if source.same_backend(destination) {
            // Resolve aliases for the destination and the source subtree.
            let actual_source = canonical(source)?;
            let actual_dest = canonical(destination)?;
            if actual_dest.starts_with(&actual_source) {
                return Err(
                    "Нельзя копировать или перемещать элемент в себя или вложенную папку".into(),
                );
            }
        }
        if kind == TransferKind::Move && (!source.remote() || !source.same_backend(destination)) {
            return Err("Перемещение между локальной папкой и SFTP или разными серверами недоступно. Скопируйте, проверьте копию и удалите оригинал отдельно.".into());
        }
        plan.push(TransferItem::new(
            source.clone(),
            source.clone(),
            destination.join(name),
        ));
    }
    Ok(plan)
}

fn keep_both_name(target: &Location, reserved: &HashSet<Location>) -> Result<Location, String> {
    let parent = target.parent().ok_or("Недопустимое назначение")?;
    let stem = target.path().file_stem().ok_or("Нет имени объекта")?;
    for index in 2..=10_000 {
        let mut name = stem.to_os_string();
        name.push(format!(" ({index})"));
        if let Some(ext) = target.path().extension() {
            name.push(".");
            name.push(ext);
        }
        let candidate = parent.join(name);
        if !reserved.contains(&candidate) && entry_metadata(&candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    Err(format!("Не удалось подобрать свободное имя для {target}"))
}

pub fn transfer_with_conflicts(
    sources: &[Location],
    destination: &Location,
    kind: TransferKind,
    progress: &mut Progress,
    mut decide: impl FnMut(Conflict) -> Result<Answer, String>,
    mut moved: impl FnMut(&Location),
) -> Result<TransferSummary, String> {
    let plan = transfer_plan(sources, destination, kind)?;
    let mut reserved: HashSet<_> = plan.iter().map(|item| item.target.clone()).collect();
    let targets = plan
        .iter()
        .map(|item| entry_metadata(&item.target))
        .collect::<Result<Vec<_>, _>>()?;
    let count = targets.iter().flatten().count();
    let mut index = 0;
    let mut group = None;
    let mut ready = Vec::with_capacity(plan.len());
    let mut summary = TransferSummary::default();

    for (mut item, target_meta) in plan.into_iter().zip(targets) {
        if let Some(target_meta) = target_meta {
            index += 1;
            let source_meta = entry_metadata(&item.source)?
                .ok_or_else(|| format!("Исходный объект {} исчез", item.source))?;
            let can_replace = source_meta.is_file()
                && target_meta.is_file()
                && !source_meta.same_object(&target_meta)
                && (!item.source.same_backend(&item.target)
                    || canonical(&item.source)? != canonical(&item.target)?);
            let decision = match group {
                Some(Decision::Replace) if !can_replace => None,
                saved => saved,
            };
            let decision = if let Some(decision) = decision {
                decision
            } else {
                progress.waiting_for_conflict(item.source.path());
                let answer = decide(Conflict {
                    source: source_meta.details(&item.original),
                    target: target_meta.details(&item.target),
                    can_replace,
                    index,
                    count,
                })?;
                progress.item(item.source.path());
                if answer.apply_to_group {
                    group = Some(answer.decision);
                }
                answer.decision
            };
            match decision {
                Decision::Cancel => {
                    summary.cancelled = true;
                    return Ok(summary);
                }
                Decision::Skip => {
                    summary.skipped += 1;
                    continue;
                }
                Decision::KeepBoth => {
                    item.target = keep_both_name(&item.target, &reserved)?;
                    reserved.insert(item.target.clone());
                }
                Decision::Replace if can_replace => {
                    item.replacement = Some((source_meta, target_meta));
                }
                Decision::Replace => {
                    return Err("Замена доступна только для двух разных обычных файлов".into());
                }
            }
        }
        ready.push(item);
    }

    for item in ready {
        progress.item(item.source.path());
        let expected = item.replacement.as_ref().map(|(_, target)| target);
        if let Some((expected_source, _)) = &item.replacement {
            if !entry_metadata(&item.source)?
                .is_some_and(|meta| expected_source.matches_file(&meta))
            {
                return Err(format!(
                    "Исходный файл {} изменился после показа диалога. Завершено объектов: {}.",
                    item.source, summary.completed
                ));
            }
        }
        let result = match kind {
            TransferKind::Copy => copy_item(&item, expected, progress),
            TransferKind::Move => publish_item(&item.source, &item.target, expected, || {
                moved(&item.original);
            }),
        };
        if let Err(error) = result {
            let completed = summary.completed;
            return Err(format!("{error}. До текущего элемента завершено: {completed}. Обновите обе папки перед повтором."));
        }
        summary.completed += 1;
    }
    Ok(summary)
}

#[cfg(test)]
pub fn transfer(
    sources: &[Location],
    destination: &Location,
    kind: TransferKind,
) -> Result<usize, String> {
    transfer_with_conflicts(
        sources,
        destination,
        kind,
        &mut Progress::default(),
        |conflict| {
            Err(format!(
                "«{}» уже существует; перезапись запрещена",
                conflict.target.location
            ))
        },
        |_| {},
    )
    .map(|report| report.completed)
}

fn canonical(location: &Location) -> Result<PathBuf, String> {
    if let Some(path) = location.local() {
        fs::canonicalize(path).map_err(err)
    } else {
        remote(location)?.realpath(location.path()).map_err(err)
    }
}
fn copy_item(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    progress: &mut Progress,
) -> Result<(), String> {
    if let (Some(source), Some(target)) = (item.source.local(), item.target.local()) {
        return fs_ops::copy_atomically_with_progress(source, target, progress, |payload, _| {
            publish_item(&payload.to_path_buf().into(), &item.target, expected, || {})
        });
    }
    copy_transfer_checked(item, expected, progress, &mut |_, _| Ok(()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyPhase {
    Opened,
    Read,
    EntryCopied,
}

fn copy_transfer_checked(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    progress: &mut Progress,
    observe: &mut impl FnMut(&Location, CopyPhase) -> Result<(), String>,
) -> Result<(), String> {
    let manifest = TransferSourceManifest::capture(&item.source)?;
    with_staging(&item.target, |staging, preserve| {
        check_copy_space(manifest.bytes, &item.target, staging)?;
        let payload = staging.join("payload");
        copy_tree(&item.source, &payload, progress, &manifest, observe)?;
        manifest.verify()?;
        if let Some(expected) = expected {
            publish_replacement(&payload, &item.target, staging, expected, preserve)
        } else {
            rename_without_replacing(&payload, &item.target)
        }
    })
}

// The old destination remains allocated throughout staging: do not subtract its
// size, even for replacement. Re-run for every item as earlier copies use space.
fn check_copy_space(required: u64, target: &Location, staging: &Location) -> Result<(), String> {
    let parent = target.parent().ok_or("Недопустимое назначение")?;
    let available = if let Some(path) = parent.local() {
        fs_ops::available_copy_space(path)?
    } else {
        // OpenSSH rejects fstatvfs on directory handles on some platforms.
        // A private, empty probe uses the destination filesystem without writing
        // payload bytes or opening/truncating an existing user file.
        let sftp = remote(staging)?;
        let probe = staging.join("space-probe");
        let mut handle = sftp
            .open_mode(
                probe.path(),
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                0o600,
                OpenType::File,
            )
            .map_err(err)?;
        let stat = handle.statvfs();
        handle.close().map_err(err)?;
        sftp.unlink(probe.path()).map_err(err)?;
        let stat = stat.map_err(|e| format!("Не удалось проверить свободное место SFTP для {parent}: {e}. Сервер должен поддерживать fstatvfs@openssh.com; данные текущего объекта не записаны"))?;
        u128::from(stat.f_bavail) * u128::from(stat.f_frsize)
    };
    fs_ops::ensure_copy_space(required, available)
}

fn rename_without_replacing(source: &Location, target: &Location) -> Result<(), String> {
    if let (Some(source), Some(target)) = (source.local(), target.local()) {
        fs_ops::rename_no_replace(source, target)
            .map_err(|e| fs_ops::rename_error(source, target, e))
    } else {
        remote(source)?
            .rename(source.path(), target.path(), Some(RenameFlags::empty()))
            .map_err(err)
    }
}

fn publish_item(
    payload: &Location,
    target: &Location,
    expected: Option<&EntryMetadata>,
    published: impl FnOnce(),
) -> Result<(), String> {
    if let Some(expected) = expected {
        with_staging(target, |staging, preserve| {
            publish_replacement(payload, target, staging, expected, preserve)?;
            published();
            Ok(())
        })
    } else {
        rename_without_replacing(payload, target)?;
        published();
        Ok(())
    }
}

fn publish_replacement(
    payload: &Location,
    target: &Location,
    staging: &Location,
    expected: &EntryMetadata,
    preserve: &mut bool,
) -> Result<(), String> {
    let target_changed = || format!("Назначение {target} изменилось после показа диалога");
    if !entry_metadata(target)?.is_some_and(|meta| expected.matches_file(&meta)) {
        return Err(target_changed());
    }
    let backup = staging.join("previous");
    // A lost SFTP reply can hide a successful rename. Preserve the staging
    // directory until the old file is either restored or safely replaced.
    *preserve = target.remote();
    rename_without_replacing(target, &backup)
        .map_err(|e| format!("{e}. Проверьте прежний файл в {backup}"))?;
    *preserve = true;
    let result = (|| {
        if !entry_metadata(&backup)?.is_some_and(|meta| expected.matches_file(&meta)) {
            return Err(target_changed());
        }
        if !entry_metadata(payload)?.is_some_and(|meta| meta.is_file()) {
            return Err(format!(
                "Исходный объект {payload} больше не является обычным файлом"
            ));
        }
        rename_without_replacing(payload, target)
    })();
    if let Err(error) = result {
        if let Err(rollback) = rename_without_replacing(&backup, target) {
            return Err(format!(
                "{error}. Не удалось восстановить прежний файл: {rollback}. Проверьте сохранённую версию в {backup}."
            ));
        }
        *preserve = false;
        return Err(error);
    }
    *preserve = false;
    Ok(())
}

fn with_staging(
    target: &Location,
    work: impl FnOnce(&Location, &mut bool) -> Result<(), String>,
) -> Result<(), String> {
    let parent = target.parent().ok_or("Недопустимое назначение")?;
    let mut preserve = false;
    if let Some(parent) = parent.local() {
        let mut builder = tempfile::Builder::new();
        builder.prefix(".pawcommander-transfer-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let staging = builder.tempdir_in(parent).map_err(err)?;
        let location = Location::Local(staging.path().into());
        let result = work(&location, &mut preserve);
        if preserve {
            let _ = staging.keep();
            return result;
        }
        return staging_result(result, staging.close().map_err(err), &location, target);
    }

    let sftp = remote(target)?;
    let mut staging = None;
    for counter in 0..100 {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let candidate = parent.join(format!(
            ".pawcommander-sftp-{}-{nonce}-{counter}",
            std::process::id()
        ));
        match sftp.mkdir(candidate.path(), 0o700) {
            Ok(()) => {
                staging = Some(candidate);
                break;
            }
            Err(e) if sftp.lstat(candidate.path()).is_err() => return Err(err(e)),
            Err(_) => {}
        }
    }
    let staging = staging.ok_or("Не удалось создать временную папку SFTP")?;
    let result = work(&staging, &mut preserve);
    if preserve {
        return result;
    }
    let cleanup = remove_remote_tree(&sftp, staging.path(), 0, &mut Progress::default());
    staging_result(result, cleanup, &staging, target)
}

fn staging_result(
    result: Result<(), String>,
    cleanup: Result<(), String>,
    staging: &Location,
    target: &Location,
) -> Result<(), String> {
    match cleanup {
        Ok(()) => result,
        Err(e) => Err(format!(
            "{}. Осталась временная папка {staging}: {e}",
            result
                .err()
                .unwrap_or_else(|| format!("Объект {target} опубликован"))
        )),
    }
}

#[cfg(test)]
fn staged_copy(source: &Location, target: &Location) -> Result<(), String> {
    copy_item(
        &TransferItem::new(source.clone(), source.clone(), target.clone()),
        None,
        &mut Progress::default(),
    )
}
fn changed_source(source: &Location) -> String {
    format!("Источник {source} изменился во время копирования; копия не опубликована. Повторите попытку после завершения записи в источник")
}

fn remote_source_matches(expected: &FileStat, actual: &FileStat) -> bool {
    // SFTP v3 exposes seconds, no inode or ctime. None is not evidence that a
    // regular file stayed unchanged: require at least type, size and mtime.
    expected.perm.is_some()
        && actual.perm.is_some()
        && (!expected.is_file() || (expected.size.is_some() && expected.mtime.is_some()))
        && expected.size == actual.size
        && expected.mtime == actual.mtime
        && expected.perm == actual.perm
        && expected.uid == actual.uid
        && expected.gid == actual.gid
}

struct TransferSourceEntry {
    metadata: EntryMetadata,
    children: Option<Vec<OsString>>,
}

impl TransferSourceEntry {
    fn verify_metadata(&self, source: &Location, actual: &EntryMetadata) -> Result<(), String> {
        let matches = match (&self.metadata, actual) {
            (EntryMetadata::Local(expected), EntryMetadata::Local(actual)) => {
                fs_ops::source_metadata_matches(expected, actual).map_err(err)?
            }
            (EntryMetadata::Remote(expected), EntryMetadata::Remote(actual)) => {
                remote_source_matches(expected, actual)
            }
            _ => false,
        };
        if matches {
            Ok(())
        } else {
            Err(changed_source(source))
        }
    }

    fn verify(&self, source: &Location) -> Result<(), String> {
        let actual = entry_metadata(source)?.ok_or_else(|| changed_source(source))?;
        self.verify_metadata(source, &actual)?;
        if let Some(children) = &self.children {
            if *children != source_children(source)? {
                return Err(changed_source(source));
            }
        }
        Ok(())
    }

    fn size(&self) -> u64 {
        match &self.metadata {
            EntryMetadata::Local(meta) => meta.len(),
            EntryMetadata::Remote(stat) => stat.size.expect("validated file size"),
        }
    }
}

fn validate_source_child_name(source: &Location, name: &OsStr) -> Result<(), String> {
    let name = name
        .to_str()
        .ok_or_else(|| format!("SFTP-передача не поддерживает имя не UTF-8 в {source}"))?;
    validate_name(name)
        .map_err(|error| format!("Неподдерживаемое имя для передачи SFTP в {source}: {error}"))
}

fn source_children(source: &Location) -> Result<Vec<OsString>, String> {
    let mut names = if let Some(path) = source.local() {
        fs::read_dir(path)
            .map_err(err)?
            .map(|entry| entry.map(|entry| entry.file_name()).map_err(err))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        remote(source)?
            .readdir(source.path())
            .map_err(err)?
            .into_iter()
            .map(|(path, _)| {
                // Use raw names, not the UI's lossy UTF-8 display names. Refuse
                // unexpected server paths instead of traversing outside source.
                let name = path.strip_prefix(source.path()).map_err(err)?;
                let mut parts = name.components();
                match (parts.next(), parts.next()) {
                    (Some(Component::Normal(name)), None) => Ok(name.to_os_string()),
                    _ => Err(format!("Недопустимое имя SFTP: {}", path.display())),
                }
            })
            .collect::<Result<Vec<_>, String>>()?
    };
    // All SFTP transfer directions share the UI/cleanup name contract. Keep
    // raw names for comparison, but reject unsupported names before staging;
    // otherwise we could create a tree that neither listing nor cleanup reads.
    // Local -> local copies use fs_ops and retain their native name support.
    for name in &names {
        validate_source_child_name(source, name)?;
    }
    names.sort();
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(format!("Повторяющиеся имена в источнике {source}"));
    }
    Ok(names)
}

struct TransferSourceManifest {
    entries: BTreeMap<Location, TransferSourceEntry>,
    bytes: u64,
}
impl TransferSourceManifest {
    fn capture(source: &Location) -> Result<Self, String> {
        let mut manifest = Self {
            entries: BTreeMap::new(),
            bytes: 0,
        };
        manifest.capture_entry(source, 0)?;
        Ok(manifest)
    }

    fn capture_entry(&mut self, source: &Location, depth: usize) -> Result<(), String> {
        if depth > 128 {
            return Err("Слишком большая глубина папок (более 128)".into());
        }
        let metadata = entry_metadata(source)?.ok_or_else(|| changed_source(source))?;
        let (is_dir, is_file) = match &metadata {
            EntryMetadata::Local(meta) => {
                meta.modified().map_err(err)?;
                (meta.is_dir(), meta.is_file())
            }
            EntryMetadata::Remote(stat) => {
                if stat.perm.is_none()
                    || (stat.is_file() && (stat.size.is_none() || stat.mtime.is_none()))
                {
                    return Err(format!("Сервер не сообщил тип, размер или время изменения {source}; безопасная проверка источника недоступна"));
                }
                (stat.is_dir(), stat.is_file())
            }
        };
        if !is_dir && !is_file {
            return Err(format!(
                "SFTP-копирование ссылок и специальных файлов пока недоступно: {source}"
            ));
        }
        let children = if is_dir {
            Some(source_children(source)?)
        } else {
            None
        };
        let entry = TransferSourceEntry { metadata, children };
        if is_file {
            self.bytes = self
                .bytes
                .checked_add(entry.size())
                .ok_or("Слишком большой размер копируемого дерева")?;
        }
        if let Some(children) = &entry.children {
            for name in children {
                self.capture_entry(&source.join(name), depth + 1)?;
            }
        }
        entry.verify(source)?;
        self.entries.insert(source.clone(), entry);
        Ok(())
    }

    fn verify(&self) -> Result<(), String> {
        // Revisit early files too; parent mtime does not track child contents.
        for (source, entry) in &self.entries {
            entry.verify(source)?;
        }
        Ok(())
    }
}

enum SourceReader {
    Local(fs::File),
    Remote(ssh2::File),
}
impl SourceReader {
    fn metadata(&mut self) -> Result<EntryMetadata, String> {
        match self {
            Self::Local(input) => input.metadata().map(EntryMetadata::Local).map_err(err),
            Self::Remote(input) => input.stat().map(EntryMetadata::Remote).map_err(|e| {
                format!("Не удалось проверить открытый исходный файл SFTP (fstat): {e}")
            }),
        }
    }
}
impl Read for SourceReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Local(input) => input.read(buffer),
            Self::Remote(input) => input.read(buffer),
        }
    }
}

fn copy_tree(
    source: &Location,
    target: &Location,
    progress: &mut Progress,
    manifest: &TransferSourceManifest,
    observe: &mut impl FnMut(&Location, CopyPhase) -> Result<(), String>,
) -> Result<(), String> {
    progress.item(source.path());
    let entry = manifest
        .entries
        .get(source)
        .ok_or_else(|| changed_source(source))?;
    entry.verify(source)?;
    if let Some(children) = &entry.children {
        if let Some(path) = target.local() {
            fs::create_dir(path).map_err(err)?;
        } else {
            remote(target)?.mkdir(target.path(), 0o755).map_err(err)?;
        }
        for name in children {
            copy_tree(
                &source.join(name),
                &target.join(name),
                progress,
                manifest,
                observe,
            )?;
        }
        return observe(source, CopyPhase::EntryCopied);
    }
    let mut input = if let Some(path) = source.local() {
        SourceReader::Local(fs_ops::open_regular_source(path).map_err(err)?)
    } else {
        SourceReader::Remote(remote(source)?.open(source.path()).map_err(err)?)
    };
    observe(source, CopyPhase::Opened)?;
    entry.verify_metadata(source, &input.metadata()?)?;
    // Bound reads to the captured size: a continuously growing source cannot
    // turn a copy into an unbounded stream. Handle/path checks detect growth.
    let expected_size = entry.size();
    let mut bounded = (&mut input).take(expected_size);
    let written = if let Some(path) = target.local() {
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(err)?;
        let written = progress
            .copy(
                source.path(),
                Some(expected_size),
                &mut bounded,
                &mut output,
            )
            .map_err(err)?;
        output.sync_all().map_err(err)?;
        written
    } else {
        let mut output = remote(target)?
            .open_mode(
                target.path(),
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                0o644,
                OpenType::File,
            )
            .map_err(err)?;
        let written = progress
            .copy(
                source.path(),
                Some(expected_size),
                &mut bounded,
                &mut output,
            )
            .map_err(err)?;
        output.flush().map_err(err)?;
        output.close().map_err(err)?;
        written
    };
    observe(source, CopyPhase::Read)?;
    if written != expected_size {
        return Err(changed_source(source));
    }
    entry.verify_metadata(source, &input.metadata()?)?;
    entry.verify(source)?;
    observe(source, CopyPhase::EntryCopied)
}

fn remove_remote_tree(
    sftp: &Sftp,
    path: &Path,
    depth: usize,
    progress: &mut Progress,
) -> Result<(), String> {
    progress.item(path);
    if depth > 128 {
        return Err("Слишком большая глубина папок".into());
    }
    let stat = sftp.lstat(path).map_err(err)?;
    if stat.is_dir() {
        for (child, _) in sftp.readdir(path).map_err(err)? {
            let name = child
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("Некорректное имя SFTP")?;
            validate_name(name)?;
            remove_remote_tree(sftp, &path.join(name), depth + 1, progress)?;
        }
        progress.item(path);
        sftp.rmdir(path).map_err(err)
    } else {
        sftp.unlink(path).map_err(err)
    }
}
#[cfg(test)]
pub fn trash_entries(paths: &[Location]) -> Result<usize, String> {
    trash_entries_with_progress(paths, &mut Progress::default())
}

pub fn trash_entries_with_progress(
    paths: &[Location],
    progress: &mut Progress,
) -> Result<usize, String> {
    if paths.iter().any(Location::remote) {
        if paths.iter().any(|p| {
            !p.remote()
                || p.parent().is_none()
                || p.path() != normalize_remote(p.path()).unwrap_or_default()
        }) {
            return Err("Недопустимый путь удаления SFTP".into());
        }
        for (completed, path) in paths.iter().enumerate() {
            progress.item(path.path());
            remove_remote_tree(&remote(path)?, path.path(), 0, progress).map_err(|e| format!("{e}. Удалено целых элементов: {completed}; текущая папка могла быть удалена частично."))?;
        }
        return Ok(paths.len());
    }
    fs_ops::trash_entries(
        &paths.iter().map(|p| p.path().into()).collect::<Vec<_>>(),
        progress,
    )
}

pub fn preview_data(
    location: &Location,
    limit: usize,
) -> Result<(ssh2::FileStat, Vec<u8>), String> {
    let sftp = remote(location)?;
    let stat = sftp.lstat(location.path()).map_err(err)?;
    if stat.is_dir() {
        return Ok((stat, Vec::new()));
    }
    if !stat.is_file() {
        return Err("Предпросмотр ссылок и специальных файлов SFTP недоступен".into());
    }
    let mut input = sftp.open(location.path()).map_err(err)?;
    let opened = input.stat().map_err(err)?;
    if !opened.is_file() {
        return Err("Файл изменился перед чтением".into());
    }
    let mut bytes = Vec::new();
    input
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    Ok((opened, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_source_comparison_requires_observable_metadata_and_ignores_atime() {
        let expected = FileStat {
            size: Some(8),
            uid: Some(1000),
            gid: Some(1000),
            perm: Some(0o100644),
            atime: Some(10),
            mtime: Some(20),
        };
        let mut actual = expected.clone();
        actual.atime = Some(999);
        assert!(remote_source_matches(&expected, &actual));
        for change in [
            "size",
            "mtime",
            "type",
            "permissions",
            "uid",
            "gid",
            "missing-size",
            "missing-mtime",
            "missing-type",
        ] {
            let mut actual = expected.clone();
            match change {
                "size" => actual.size = Some(9),
                "mtime" => actual.mtime = Some(21),
                "type" => actual.perm = Some(0o120644),
                "permissions" => actual.perm = Some(0o100600),
                "uid" => actual.uid = Some(1001),
                "gid" => actual.gid = Some(1001),
                "missing-size" => actual.size = None,
                "missing-mtime" => actual.mtime = None,
                "missing-type" => actual.perm = None,
                _ => unreachable!(),
            }
            assert!(!remote_source_matches(&expected, &actual), "{change}");
            if change.starts_with("missing") {
                assert!(!remote_source_matches(&actual, &actual), "{change}");
            }
        }
        // Identical observable metadata cannot distinguish a same-second edit
        // or an inode replacement on SFTP v3. Do not claim snapshot semantics.
        assert!(remote_source_matches(&expected, &expected));
    }

    #[test]
    fn transfer_source_final_check_catches_early_edits_and_preserves_target() {
        let root = tempfile::tempdir().unwrap();
        let source = Location::Local(root.path().join("source"));
        let target = Location::Local(root.path().join("target"));
        fs::create_dir(source.path()).unwrap();
        fs::write(source.join("a-early").path(), b"early").unwrap();
        fs::write(source.join("z-late").path(), b"late").unwrap();
        fs::write(target.path(), b"previous").unwrap();
        let expected = entry_metadata(&target).unwrap().unwrap();
        let error = copy_transfer_checked(
            &TransferItem::new(source.clone(), source.clone(), target.clone()),
            Some(&expected),
            &mut Progress::default(),
            &mut |path, phase| {
                if path == &source.join("z-late") && phase == CopyPhase::EntryCopied {
                    fs::write(source.join("a-early").path(), b"changed early").unwrap();
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.contains("изменился"), "{error}");
        assert_eq!(fs::read(target.path()).unwrap(), b"previous");
        assert_eq!(
            fs::read(source.join("a-early").path()).unwrap(),
            b"changed early"
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[cfg(unix)]
    fn unsupported_sftp_names() -> Vec<OsString> {
        // macOS filesystems reject invalid UTF-8 at creation time; test those
        // raw bytes separately below without requiring a compatible volume.
        vec![OsString::from("bad\\name"), OsString::from(" ")]
    }

    #[cfg(unix)]
    #[test]
    fn sftp_name_validation_rejects_non_utf8_without_lossy_conversion() {
        use std::os::unix::ffi::OsStringExt;
        let source = Location::Local(PathBuf::from("source"));
        let raw = OsString::from_vec(b"bad-\xff-name".to_vec());
        assert!(validate_source_child_name(&source, &raw)
            .unwrap_err()
            .contains("не UTF-8"));
        // A literal replacement character is valid Unicode, but must never
        // substitute for the invalid byte and select a different source file.
        assert!(validate_source_child_name(&source, OsStr::new("bad-�-name")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn sftp_manifest_rejects_unsupported_nested_names_before_staging() {
        for name in unsupported_sftp_names() {
            let root = tempfile::tempdir().unwrap();
            let source = Location::Local(root.path().join("source"));
            let target = Location::Local(root.path().join("target"));
            fs::create_dir_all(source.join("nested").path()).unwrap();
            let unsupported = source.join("nested").join(&name);
            fs::write(unsupported.path(), b"source bytes").unwrap();
            fs::write(target.path(), b"previous target").unwrap();
            let expected = entry_metadata(&target).unwrap().unwrap();
            let error = copy_transfer_checked(
                &TransferItem::new(source.clone(), source.clone(), target.clone()),
                Some(&expected),
                &mut Progress::default(),
                &mut |_, _| panic!("unsupported names must fail before copying"),
            )
            .unwrap_err();
            assert!(error.contains("SFTP"), "{error}");
            assert!(!error.contains("Осталась"), "{error}");
            assert_eq!(fs::read(target.path()).unwrap(), b"previous target");
            assert_eq!(fs::read(unsupported.path()).unwrap(), b"source bytes");
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);

            // The stricter contract applies only to SFTP transfers. A native
            // local copy still preserves names supported by the local volume.
            let local_target =
                create_directory(&root.path().to_path_buf().into(), "local-copy").unwrap();
            transfer(
                std::slice::from_ref(&source),
                &local_target,
                TransferKind::Copy,
            )
            .unwrap();
            assert_eq!(
                fs::read(local_target.join("source/nested").join(&name).path()).unwrap(),
                b"source bytes"
            );
        }
    }

    #[cfg(unix)]
    fn check_sftp_unsupported_names(local_root: &Location, remote_root: &Location) {
        for direction in ["upload", "download", "remote-copy"] {
            let (source_root, target_root) = match direction {
                "upload" => (local_root, remote_root),
                "download" => (remote_root, local_root),
                _ => (remote_root, remote_root),
            };
            for (index, name) in unsupported_sftp_names().into_iter().enumerate() {
                for late in [false, true] {
                    let source =
                        source_root.join(format!("invalid-name-{direction}-{index}-{late}-source"));
                    let target =
                        target_root.join(format!("invalid-name-{direction}-{index}-{late}-target"));
                    fs::create_dir_all(source.join("nested").path()).unwrap();
                    fs::write(source.join("nested/valid").path(), b"valid source").unwrap();
                    let unsupported = source.join("nested").join(&name);
                    if !late {
                        fs::write(unsupported.path(), b"unsupported source").unwrap();
                    }
                    fs::write(target.path(), b"previous destination").unwrap();
                    let expected = entry_metadata(&target).unwrap().unwrap();
                    let mut copied = false;
                    let error = copy_transfer_checked(
                        &TransferItem::new(source.clone(), source.clone(), target.clone()),
                        Some(&expected),
                        &mut Progress::default(),
                        &mut |path, phase| {
                            assert!(late, "preflight must reject names before copying");
                            if path == &source && phase == CopyPhase::EntryCopied {
                                copied = true;
                                fs::write(unsupported.path(), b"unsupported source").unwrap();
                            }
                            Ok(())
                        },
                    )
                    .unwrap_err();
                    assert_eq!(copied, late);
                    // A directory metadata change may be observed before its
                    // new unsupported name; either must abort and clean up.
                    assert!(
                        error.contains("SFTP") || error.contains("изменился"),
                        "{error}"
                    );
                    assert!(
                        !error.contains("Осталась"),
                        "{direction}/{index}/{late}: {error}"
                    );
                    assert_eq!(fs::read(target.path()).unwrap(), b"previous destination");
                    assert_eq!(fs::read(unsupported.path()).unwrap(), b"unsupported source");
                    assert_eq!(
                        fs::read(source.join("nested/valid").path()).unwrap(),
                        b"valid source"
                    );
                    assert!(fs::read_dir(target_root.path()).unwrap().all(|entry| {
                        let name = entry.unwrap().file_name();
                        let name = name.to_string_lossy();
                        !name.starts_with(".pawcommander-sftp-")
                            && !name.starts_with(".pawcommander-transfer-")
                    }));
                    // The destination parent remains readable by the UI.
                    read_directory(target_root).unwrap();
                }
            }
        }
    }

    // Uses only disposable loopback fixtures. Mutations are synchronized with
    // copy checkpoints rather than sleeps or probabilistic background writers.
    fn check_sftp_source_changes(local_root: &Location, remote_root: &Location) {
        for direction in ["upload", "download", "remote-copy"] {
            let (source_root, target_root) = match direction {
                "upload" => (local_root, remote_root),
                "download" => (remote_root, local_root),
                _ => (remote_root, remote_root),
            };
            for mutation in [
                "append",
                "truncate",
                "same-size-mtime",
                "opened-handle",
                "early-file",
                "add-child",
                "remove-child",
                "rename-child",
            ] {
                let source = source_root.join(format!("{direction}-{mutation}-source"));
                let target = target_root.join(format!("{direction}-{mutation}-target"));
                let tree = matches!(
                    mutation,
                    "early-file" | "add-child" | "remove-child" | "rename-child"
                );
                if tree {
                    fs::create_dir_all(source.join("a-nested").path()).unwrap();
                    fs::write(source.join("a-nested/early").path(), b"early").unwrap();
                    fs::write(source.join("z-late").path(), b"late").unwrap();
                } else {
                    fs::write(source.path(), b"original").unwrap();
                }
                fs::write(target.path(), b"previous destination").unwrap();
                let expected = entry_metadata(&target).unwrap().unwrap();
                let mut mutated = false;
                let error = copy_transfer_checked(
                    &TransferItem::new(source.clone(), source.clone(), target.clone()),
                    Some(&expected),
                    &mut Progress::default(),
                    &mut |path, phase| {
                        let checkpoint = if mutation == "opened-handle" {
                            CopyPhase::Opened
                        } else if tree {
                            CopyPhase::EntryCopied
                        } else {
                            CopyPhase::Read
                        };
                        let path_matches = if tree {
                            path == &source.join("z-late")
                        } else {
                            path == &source
                        };
                        if mutated || phase != checkpoint || !path_matches {
                            return Ok(());
                        }
                        mutated = true;
                        match mutation {
                            "append" => fs::write(source.path(), b"original grew").unwrap(),
                            "truncate" => fs::write(source.path(), b"short").unwrap(),
                            "same-size-mtime" => {
                                fs::write(source.path(), b"modified").unwrap();
                                fs::File::options()
                                    .write(true)
                                    .open(source.path())
                                    .unwrap()
                                    .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
                                    .unwrap();
                            }
                            "opened-handle" => {
                                let metadata = fs::metadata(source.path()).unwrap();
                                let retained =
                                    source_root.join(format!("{direction}-retained-inode"));
                                fs::rename(source.path(), retained.path()).unwrap();
                                fs::write(source.path(), b"original").unwrap();
                                fs::File::options()
                                    .write(true)
                                    .open(source.path())
                                    .unwrap()
                                    .set_times(
                                        fs::FileTimes::new()
                                            .set_modified(metadata.modified().unwrap()),
                                    )
                                    .unwrap();
                                // lstat now sees an indistinguishable SFTP file,
                                // but fstat must see the changed open inode.
                                fs::write(retained.path(), b"changed opened inode").unwrap();
                            }
                            "early-file" => {
                                fs::write(source.join("a-nested/early").path(), b"early changed")
                                    .unwrap()
                            }
                            "add-child" => {
                                fs::write(source.join("a-nested/new").path(), b"new").unwrap()
                            }
                            "remove-child" => {
                                fs::remove_file(source.join("a-nested/early").path()).unwrap()
                            }
                            "rename-child" => fs::rename(
                                source.join("a-nested/early").path(),
                                source.join("a-nested/renamed").path(),
                            )
                            .unwrap(),
                            _ => unreachable!(),
                        }
                        Ok(())
                    },
                )
                .unwrap_err();
                assert!(mutated, "{direction}/{mutation}");
                assert!(
                    error.contains("изменился"),
                    "{direction}/{mutation}: {error}"
                );
                assert_eq!(fs::read(target.path()).unwrap(), b"previous destination");
                assert!(source.path().exists());
                assert!(fs::read_dir(target_root.path()).unwrap().all(|entry| {
                    let name = entry.unwrap().file_name();
                    let name = name.to_string_lossy();
                    !name.starts_with(".pawcommander-sftp-")
                        && !name.starts_with(".pawcommander-transfer-")
                }));
            }
        }
    }

    #[test]
    fn conflict_group_replaces_files_and_asks_again_for_a_directory() {
        let root = tempfile::tempdir().unwrap();
        let root: Location = root.path().to_path_buf().into();
        let source = create_directory(&root, "source").unwrap();
        let target = create_directory(&root, "target").unwrap();
        let names = ["first.txt", "second.txt", "project.dir", "project (2).dir"];
        let sources: Vec<_> = names.iter().map(|name| source.join(name)).collect();
        for name in &names[..2] {
            fs::write(source.join(name).path(), "new bytes").unwrap();
            fs::write(target.join(name).path(), "old").unwrap();
        }
        fs::create_dir(source.join(names[2]).path()).unwrap();
        fs::create_dir(target.join(names[2]).path()).unwrap();
        fs::write(source.join(names[2]).join("nested.txt").path(), "nested").unwrap();
        fs::write(source.join(names[3]).path(), "reserved").unwrap();

        let mut prompted = Vec::new();
        let result = transfer_with_conflicts(
            &sources,
            &target,
            TransferKind::Copy,
            &mut Progress::default(),
            |conflict| {
                prompted.push(conflict.index);
                assert_eq!(conflict.count, 3);
                if conflict.index == 1 {
                    assert!(conflict.can_replace);
                    assert_eq!(conflict.source.size, Some(9));
                    assert_eq!(conflict.target.size, Some(3));
                    assert!(conflict.source.modified.is_some());
                    assert!(conflict.target.modified.is_some());
                    Ok(Answer {
                        decision: Decision::Replace,
                        apply_to_group: true,
                    })
                } else {
                    assert!(!conflict.can_replace);
                    Ok(Answer {
                        decision: Decision::KeepBoth,
                        apply_to_group: false,
                    })
                }
            },
            |_| panic!("copy must not report a move"),
        )
        .unwrap();
        assert_eq!(prompted, vec![1, 3]);
        assert_eq!(result.completed, 4);
        assert_eq!(result.skipped, 0);
        assert!(!result.cancelled);
        for name in &names[..2] {
            assert_eq!(fs::read(target.join(name).path()).unwrap(), b"new bytes");
        }
        assert_eq!(
            fs::read_dir(target.join(names[2]).path()).unwrap().count(),
            0
        );
        assert_eq!(
            fs::read(target.join("project (3).dir").join("nested.txt").path()).unwrap(),
            b"nested"
        );
        assert_eq!(fs::read(target.join(names[3]).path()).unwrap(), b"reserved");
    }

    #[test]
    fn conflict_cancel_leaves_the_whole_batch_and_skip_preserves_cut_sources() {
        let root = tempfile::tempdir().unwrap();
        let root: Location = root.path().to_path_buf().into();
        let source = create_directory(&root, "source").unwrap();
        let target = create_directory(&root, "target").unwrap();
        let names = ["free.txt", "blocked.txt", "another.txt"];
        let sources: Vec<_> = names.iter().map(|name| source.join(name)).collect();
        for item in &sources {
            fs::write(item.path(), "source").unwrap();
        }
        for name in &names[1..] {
            fs::write(target.join(name).path(), "destination").unwrap();
        }
        let cancelled = transfer_with_conflicts(
            &sources,
            &target,
            TransferKind::Move,
            &mut Progress::default(),
            |_| {
                Ok(Answer {
                    decision: Decision::Cancel,
                    apply_to_group: false,
                })
            },
            |_| panic!("nothing may move before conflicts are resolved"),
        )
        .unwrap();
        assert!(cancelled.cancelled);
        assert!(sources.iter().all(|item| item.path().exists()));
        assert!(!target.join(names[0]).path().exists());

        let mut moved = Vec::new();
        let report = transfer_with_conflicts(
            &sources,
            &target,
            TransferKind::Move,
            &mut Progress::default(),
            |conflict| {
                assert_eq!(conflict.index, 1);
                Ok(Answer {
                    decision: Decision::Skip,
                    apply_to_group: true,
                })
            },
            |location| moved.push(location.clone()),
        )
        .unwrap();
        assert_eq!((report.completed, report.skipped), (1, 2));
        assert_eq!(moved, sources[..1]);
        assert!(!sources[0].path().exists());
        for (source, name) in sources[1..].iter().zip(&names[1..]) {
            assert_eq!(fs::read(source.path()).unwrap(), b"source");
            assert_eq!(fs::read(target.join(name).path()).unwrap(), b"destination");
        }
    }

    #[test]
    fn conflict_replace_preserves_changed_destination_and_restores_on_publish_error() {
        let root = tempfile::tempdir().unwrap();
        let root: Location = root.path().to_path_buf().into();
        let target_dir = create_directory(&root, "target").unwrap();
        let target = target_dir.join("report.txt");
        let source = root.join("report.txt");
        fs::write(source.path(), "new").unwrap();
        fs::write(target.path(), "old").unwrap();
        let failure = transfer_with_conflicts(
            std::slice::from_ref(&source),
            &target_dir,
            TransferKind::Copy,
            &mut Progress::default(),
            |_| {
                fs::write(target.path(), "changed elsewhere").unwrap();
                Ok(Answer {
                    decision: Decision::Replace,
                    apply_to_group: false,
                })
            },
            |_| {},
        )
        .unwrap_err();
        assert!(failure.contains("изменилось после показа диалога"));
        assert_eq!(fs::read(target.path()).unwrap(), b"changed elsewhere");

        let expected = entry_metadata(&target).unwrap().unwrap();
        assert!(
            publish_item(&root.join("missing"), &target, Some(&expected), || {
                panic!("missing payload cannot be published");
            })
            .is_err()
        );
        assert_eq!(fs::read(target.path()).unwrap(), b"changed elsewhere");
        assert_eq!(fs::read_dir(target_dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn remote_addresses_round_trip_and_keep_connection_and_posix_history() {
        let local = Location::Local(PathBuf::from("/tmp"));
        let location = resolve("sftp://alice@example.com:2222/папка/two%20words", &local).unwrap();
        assert_eq!(location.path(), Path::new("/папка/two words"));
        assert_eq!(resolve(&location.display(), &local).unwrap(), location);
        assert_eq!(
            resolve("../next", &location).unwrap().path(),
            Path::new("/папка/next")
        );
        assert_eq!(
            resolve("/../../", &location).unwrap().path(),
            Path::new("/")
        );
        assert!(location.ancestors().all(|p| p.remote()));
        assert!(location.ancestors().last().unwrap().parent().is_none());
        let ipv6 = resolve("sftp://alice@[::1]:2222/a", &local).unwrap();
        assert_eq!(resolve(&ipv6.display(), &local).unwrap(), ipv6);
        let special = location.with_path(PathBuf::from("/literal%20/100% #?.txt"));
        assert_eq!(resolve(&special.display(), &local).unwrap(), special);
        let form_ipv6 = Location::Sftp {
            endpoint: Endpoint {
                host: "::1".into(),
                port: 2222,
                user: "a%40b".into(),
                key: None,
            },
            path: "/".into(),
        };
        let parsed_ipv6 = resolve(&form_ipv6.display(), &local).unwrap();
        assert_eq!(parsed_ipv6.path(), form_ipv6.path());
        if let Location::Sftp { endpoint, .. } = parsed_ipv6 {
            assert_eq!(endpoint.user, "a%40b");
        }
        assert!(resolve("sftp://alice:secret@example.com/", &local).is_err());
        assert!(resolve("sftp://example.com/", &local).is_err());
        assert!(resolve("sftp://alice@example.com/%00", &local).is_err());
        assert!(resolve("sftp://alice@example.com/?q=1", &local).is_err());
    }

    #[test]
    fn remote_key_path_survives_address_edits() {
        let current = Location::Sftp {
            endpoint: Endpoint {
                host: "example.com".into(),
                port: 22,
                user: "alice".into(),
                key: Some(PathBuf::from("/keys/id")),
            },
            path: "/".into(),
        };
        assert_eq!(
            resolve("sftp://alice@example.com/sub", &current).unwrap(),
            current.join("sub")
        );
    }

    #[test]
    fn remote_join_keeps_literal_posix_separators_on_every_client_os() {
        let local = Location::Local(PathBuf::from("/tmp"));
        let root = resolve("sftp://alice@example.com/", &local).unwrap();
        let child = root.join("folder").join("two words");
        assert_eq!(child.path().to_str(), Some("/folder/two words"));
        assert_eq!(child.join("").path().to_str(), Some("/folder/two words"));
        assert_eq!(child.join("/other").path().to_str(), Some("/other"));
        assert_eq!(
            resolve("../next", &child).unwrap().path().to_str(),
            Some("/folder/next")
        );
        assert!(resolve("..\\next", &child).is_err());
        assert!(resolve("sftp://alice@example.com/a%5Cb", &local).is_err());
        assert_eq!(local.join("folder").path(), local.path().join("folder"));
    }

    #[test]
    fn local_operations_keep_existing_conflict_and_trash_policy() {
        let root = tempfile::tempdir().unwrap();
        let local = Location::Local(root.path().into());
        let a = create_directory(&local, "A").unwrap();
        let data = "data".repeat((1 << 18) + 1);
        fs::write(a.path().join("file.txt"), &data).unwrap();
        assert!(create_directory(&local, "A").is_err());
        assert!(create_directory(&local, "../escape").is_err());
        #[cfg(windows)]
        assert!(validate_name("C:").is_err());
        let b = create_directory(&local, "B").unwrap();
        let (mut progress, mut view) = Progress::tracked();
        assert_eq!(
            transfer_with_conflicts(
                std::slice::from_ref(&a),
                &b,
                TransferKind::Copy,
                &mut progress,
                |_| panic!("unexpected conflict"),
                |_| {},
            )
            .unwrap()
            .completed,
            1
        );
        let snapshot = view.snapshot();
        assert_eq!(
            snapshot.item.as_deref().unwrap().file_name().unwrap(),
            "file.txt"
        );
        assert_eq!(snapshot.total_written, data.len() as u64);
        assert_eq!(
            fs::read_to_string(b.path().join("A/file.txt")).unwrap(),
            data
        );
        assert!(transfer(std::slice::from_ref(&a), &b, TransferKind::Copy).is_err());
        let remote = resolve("sftp://alice@example.com/a", &local).unwrap();
        assert!(trash_entries(&[remote.with_path("/".into())]).is_err());
    }

    // Run with scripts/test-sftp.sh: a disposable loopback-only OpenSSH server
    // and generated keys, without using the user's real known_hosts or keys.
    #[test]
    #[ignore = "requires disposable OpenSSH server; run scripts/test-sftp.sh"]
    fn openssh_round_trip_and_host_verification() {
        let root = PathBuf::from(
            std::env::var_os("PAWCOMMANDER_TEST_SFTP_ROOT").expect("test server root"),
        );
        let endpoint = Endpoint {
            host: "127.0.0.1".into(),
            port: std::env::var("PAWCOMMANDER_TEST_SFTP_PORT")
                .unwrap()
                .parse()
                .unwrap(),
            user: std::env::var("PAWCOMMANDER_TEST_SFTP_USER").unwrap(),
            key: Some(root.join("client")),
        };
        let known = root.join("known_hosts");
        let empty = root.join("empty_hosts");
        fs::write(&empty, "").unwrap();
        assert!(connect(&endpoint, "", "test-passphrase", &empty)
            .err()
            .unwrap()
            .contains("не подтверждён"));
        let mismatch = root.join("mismatch_hosts");
        let other_key = fs::read_to_string(root.join("other.pub")).unwrap();
        fs::write(
            &mismatch,
            format!("[127.0.0.1]:{} {}", endpoint.port, other_key),
        )
        .unwrap();
        assert!(connect(&endpoint, "", "test-passphrase", &mismatch)
            .err()
            .unwrap()
            .contains("изменился"));
        assert!(connect(&endpoint, "", "wrong-passphrase", &known).is_err());
        let memory_key = fs::read_to_string(root.join("client")).unwrap();
        let absent_key = Endpoint {
            key: Some(root.join("does-not-exist")),
            ..endpoint.clone()
        };
        assert!(connect_with_key(
            &absent_key,
            "",
            "test-passphrase",
            &known,
            Some(&memory_key)
        )
        .unwrap()
        .authenticated());
        assert!(
            connect_with_key(&absent_key, "", "wrong-phrase", &known, Some(&memory_key)).is_err()
        );
        let remote_root = Location::Sftp {
            endpoint: endpoint.clone(),
            path: root.join("remote"),
        };
        assert!(read_address_directory(&remote_root).unwrap().is_empty());
        let session = connect(&endpoint, "", "test-passphrase", &known).unwrap();
        sessions().lock().unwrap().insert(endpoint.clone(), session);
        let local_root = Location::Local(root.join("local"));
        fs::create_dir(local_root.path()).unwrap();
        fs::create_dir(remote_root.path()).unwrap();
        let completion_folder = remote_root.join("Completion folder");
        fs::create_dir(completion_folder.path()).unwrap();
        assert!(read_address_directory(&remote_root)
            .unwrap()
            .iter()
            .any(|entry| entry.is_dir && entry.path == completion_folder));
        fs::remove_dir(completion_folder.path()).unwrap();
        let source = create_directory(&local_root, "папка ' [test]").unwrap();
        fs::write(source.path().join("hello world.txt"), "Привет, SFTP!\n").unwrap();
        fs::write(source.path().join(".hidden"), "hidden").unwrap();
        fs::write(source.path().join("empty"), "").unwrap();
        fs::create_dir(source.path().join("sub")).unwrap();
        fs::write(source.path().join("sub/large.bin"), vec![0xab; 512 * 1024]).unwrap();
        if std::env::var("PAWCOMMANDER_TEST_DISABLE_STATVFS").as_deref() == Ok("1") {
            let target = remote_root.join(source.name());
            let error = staged_copy(&source, &target).unwrap_err();
            assert!(error.contains("fstatvfs@openssh.com"), "{error}");
            assert!(!target.path().exists());
            assert!(read_directory(&remote_root).unwrap().is_empty());
            assert_eq!(fs::read(source.path().join(".hidden")).unwrap(), b"hidden");
            sessions().lock().unwrap().remove(&endpoint);
            return;
        }
        // Exercise fstatvfs on the destination filesystem before upload,
        // then exercise the remote size walk used before download/replacement.
        let expected_size = 512 * 1024 + "Привет, SFTP!\n".len() as u64 + "hidden".len() as u64;
        assert_eq!(
            TransferSourceManifest::capture(&source).unwrap().bytes,
            expected_size
        );
        with_staging(&remote_root.join(source.name()), |staging, _| {
            check_copy_space(expected_size, &remote_root.join(source.name()), staging)
        })
        .unwrap();
        let (mut progress, mut view) = Progress::tracked();
        assert_eq!(
            transfer_with_conflicts(
                std::slice::from_ref(&source),
                &remote_root,
                TransferKind::Copy,
                &mut progress,
                |_| panic!("unexpected conflict"),
                |_| {},
            )
            .unwrap()
            .completed,
            1
        );
        assert_eq!(
            view.snapshot().total_written,
            512 * 1024 + "Привет, SFTP!\n".len() as u64 + "hidden".len() as u64
        );
        let entries = read_directory(&remote_root).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, source.name());
        assert!(entries[0].is_dir);
        let uploaded = entries[0].path.clone();
        assert_eq!(
            TransferSourceManifest::capture(&uploaded).unwrap().bytes,
            expected_size
        );
        check_copy_space(
            expected_size,
            &local_root.join("download-preflight"),
            &local_root,
        )
        .unwrap();
        assert_eq!(read_directory(&uploaded).unwrap().len(), 4);
        assert!(transfer(
            std::slice::from_ref(&source),
            &remote_root,
            TransferKind::Copy
        )
        .is_err());
        let downloads = create_directory(&local_root, "downloads").unwrap();
        transfer(
            std::slice::from_ref(&uploaded),
            &downloads,
            TransferKind::Copy,
        )
        .unwrap();
        assert_eq!(
            fs::read(downloads.join(source.name()).path().join("sub/large.bin")).unwrap(),
            vec![0xab; 512 * 1024]
        );
        assert_eq!(
            fs::read_to_string(downloads.join(source.name()).path().join("hello world.txt"))
                .unwrap(),
            "Привет, SFTP!\n"
        );
        let file_source = source.join("hello world.txt");
        let replace = |conflict: Conflict| {
            assert!(conflict.can_replace);
            assert!(conflict.source.size.is_some());
            assert!(conflict.target.modified.is_some());
            Ok(Answer {
                decision: Decision::Replace,
                apply_to_group: false,
            })
        };
        transfer_with_conflicts(
            std::slice::from_ref(&file_source),
            &uploaded,
            TransferKind::Copy,
            &mut Progress::default(),
            replace,
            |_| {},
        )
        .unwrap();
        assert_eq!(
            preview_data(&uploaded.join("hello world.txt"), 5)
                .unwrap()
                .1
                .len(),
            6
        );
        assert_eq!(
            preview_data(&uploaded.join("hello world.txt"), 1024)
                .unwrap()
                .1,
            "Привет, SFTP!\n".as_bytes()
        );
        fs::write(file_source.path(), "replacement").unwrap();
        transfer_with_conflicts(
            std::slice::from_ref(&file_source),
            &uploaded,
            TransferKind::Copy,
            &mut Progress::default(),
            replace,
            |_| {},
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(uploaded.join("hello world.txt").path()).unwrap(),
            "replacement"
        );
        transfer_with_conflicts(
            std::slice::from_ref(&file_source),
            &uploaded,
            TransferKind::Copy,
            &mut Progress::default(),
            |_| {
                Ok(Answer {
                    decision: Decision::KeepBoth,
                    apply_to_group: false,
                })
            },
            |_| {},
        )
        .unwrap();
        let remote_source = uploaded.join("hello world (2).txt");
        let remote_target = remote_root.join("hello world (2).txt");
        assert_eq!(fs::read(remote_source.path()).unwrap(), b"replacement");
        fs::write(remote_target.path(), "previous remote file").unwrap();
        transfer_with_conflicts(
            std::slice::from_ref(&remote_source),
            &remote_root,
            TransferKind::Move,
            &mut Progress::default(),
            replace,
            |_| {},
        )
        .unwrap();
        assert!(!remote_source.path().exists());
        assert_eq!(fs::read(remote_target.path()).unwrap(), b"replacement");
        assert!(transfer_with_conflicts(
            std::slice::from_ref(&source),
            &remote_root,
            TransferKind::Copy,
            &mut Progress::default(),
            |conflict| {
                assert!(!conflict.can_replace);
                Ok(Answer {
                    decision: Decision::Replace,
                    apply_to_group: false,
                })
            },
            |_| {},
        )
        .is_err());
        let disposable = create_directory(&remote_root, "delete-me").unwrap();
        fs::write(disposable.path().join("file"), "delete").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(source.path(), disposable.path().join("link")).unwrap();
        trash_entries(std::slice::from_ref(&disposable)).unwrap();
        assert!(!disposable.path().exists());
        assert!(source.path().exists());
        let renamed = rename_entry(&uploaded, "renamed").unwrap();
        let conflict = create_directory(&remote_root, "conflict").unwrap();
        assert!(rename_entry(&renamed, "conflict").is_err());
        assert!(remote(&conflict)
            .unwrap()
            .stat(conflict.path())
            .unwrap()
            .is_dir());
        // Direct no-replace publication must also reject an existing target.
        let file = renamed.join("hello world.txt");
        assert!(staged_copy(&file, &renamed.join(".hidden")).is_err());
        assert_eq!(
            fs::read_to_string(renamed.path().join(".hidden")).unwrap(),
            "hidden"
        );
        let sub = create_directory(&remote_root, "destination").unwrap();
        transfer(std::slice::from_ref(&renamed), &sub, TransferKind::Move).unwrap();
        let moved = sub.join("renamed");
        assert!(transfer(std::slice::from_ref(&moved), &moved, TransferKind::Copy).is_err());
        assert!(transfer(std::slice::from_ref(&moved), &downloads, TransferKind::Move).is_err());
        assert!(remote(&moved).unwrap().stat(moved.path()).unwrap().is_dir());
        assert!(transfer(
            std::slice::from_ref(&source),
            &remote_root,
            TransferKind::Move
        )
        .is_err());
        // Remote -> remote copy and refusal to follow symlinks in transfer trees.
        transfer(std::slice::from_ref(&moved), &conflict, TransferKind::Copy).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(source.path(), remote_root.path().join("alias")).unwrap();
            let link = remote_root.join("alias");
            assert!(read_directory(&remote_root)
                .unwrap()
                .iter()
                .any(|e| e.is_symlink && e.is_dir));
            assert!(transfer(&[link], &downloads, TransferKind::Copy).is_err());
            assert!(!downloads.path().join("alias").exists());
            std::os::unix::fs::symlink(source.path(), source.path().join("loop")).unwrap();
            assert!(transfer(
                std::slice::from_ref(&source),
                &remote_root,
                TransferKind::Copy
            )
            .is_err());
            assert!(!remote_root.join(source.name()).path().exists());
        }
        assert!(fs::read_dir(remote_root.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pawcommander-sftp-")));
        assert!(fs::read_dir(downloads.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pawcommander-transfer-")));
        check_sftp_source_changes(&local_root, &remote_root);
        #[cfg(unix)]
        check_sftp_unsupported_names(&local_root, &remote_root);
        sessions().lock().unwrap().remove(&endpoint);
    }
}
