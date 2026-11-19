use super::err;
use super::location::{Endpoint, Location};
use crate::{archive, i18n::text};
use ssh2::{CheckResult, KnownHostFileKind, Session, Sftp};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use zeroize::Zeroizing;

mod interactive;
pub(crate) use interactive::{AuthRequest, ConnectionControl, InteractiveAuth};

const IO_TIMEOUT_MS: u32 = 20_000;

pub(crate) struct ConnectRequest {
    pub endpoint: Endpoint,
    pub path: String,
    pub password: Zeroizing<String>,
    pub passphrase: Zeroizing<String>,
    pub use_saved: bool,
    pub save_key: bool,
}

pub(crate) struct ConnectedLocation {
    pub location: Location,
    pub saved_profile: bool,
}

pub(super) static SESSIONS: OnceLock<Mutex<HashMap<Endpoint, Session>>> = OnceLock::new();

pub(super) fn sessions() -> &'static Mutex<HashMap<Endpoint, Session>> {
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

pub(super) fn connect_with_key(
    endpoint: &Endpoint,
    password: &str,
    passphrase: &str,
    known_hosts: &Path,
    private_key: Option<&str>,
) -> Result<Session, String> {
    let session = open_verified(endpoint, known_hosts, None)?;
    PrimaryAuth::new(endpoint, password, passphrase, private_key)
        .attempt(&session, &endpoint.user)?;
    if !session.authenticated() {
        return Err(text("Сервер не подтвердил аутентификацию").into());
    }
    Ok(session)
}

fn open_verified(
    endpoint: &Endpoint,
    known_hosts: &Path,
    control: Option<&ConnectionControl>,
) -> Result<Session, String> {
    if endpoint.host.trim().is_empty() || endpoint.user.trim().is_empty() || endpoint.port == 0 {
        return Err("Укажите сервер, пользователя и порт 1–65535".into());
    }
    // Bound TCP and SSH I/O waits. DNS resolution uses the OS resolver.
    let host = endpoint.host_name();
    let addresses = (host, endpoint.port)
        .to_socket_addrs()
        .map_err(|e| format!("Не удалось найти сервер: {e}"))?;
    let mut tcp = None;
    for address in addresses {
        if let Some(control) = control {
            control.check()?;
        }
        if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_secs(10)) {
            tcp = Some(stream);
            break;
        }
    }
    let tcp = tcp.ok_or("Не удалось подключиться к серверу SFTP (тайм-аут 10 с на адрес)")?;
    if let Some(control) = control {
        control.attach(&tcp)?;
    }
    let timeout = Some(Duration::from_millis(u64::from(IO_TIMEOUT_MS)));
    tcp.set_read_timeout(timeout).map_err(err)?;
    tcp.set_write_timeout(timeout).map_err(err)?;
    let mut session = Session::new().map_err(err)?;
    session.set_tcp_stream(tcp);
    session.set_timeout(IO_TIMEOUT_MS);
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
    Ok(session)
}

enum PrimaryAuth<'a> {
    Password(&'a str),
    MemoryKey(&'a str, Option<&'a str>),
    KeyFile(&'a Path, Option<&'a str>),
    Agent,
}

impl<'a> PrimaryAuth<'a> {
    fn new(
        endpoint: &'a Endpoint,
        password: &'a str,
        passphrase: &'a str,
        private_key: Option<&'a str>,
    ) -> Self {
        let phrase = (!passphrase.is_empty()).then_some(passphrase);
        if !password.is_empty() {
            Self::Password(password)
        } else if let Some(key) = private_key {
            Self::MemoryKey(key, phrase)
        } else if let Some(key) = &endpoint.key {
            Self::KeyFile(key, phrase)
        } else {
            Self::Agent
        }
    }

    fn method(&self) -> &'static str {
        match self {
            Self::Password(_) => "password",
            _ => "publickey",
        }
    }

    fn is_explicit_key(&self) -> bool {
        matches!(self, Self::MemoryKey(..) | Self::KeyFile(..))
    }

    fn attempt(&self, session: &Session, user: &str) -> Result<(), String> {
        match *self {
            Self::Password(password) => session.userauth_password(user, password).map_err(err),
            Self::MemoryKey(key, phrase) => session.userauth_pubkey_memory(user, None, key, phrase).map_err(err),
            Self::KeyFile(key, phrase) => session.userauth_pubkey_file(user, None, key, phrase).map_err(err),
            Self::Agent => session.userauth_agent(user).map_err(|error| {
                crate::tr_format!(
                    "SSH-agent ({error}). Добавьте ключ в agent или укажите ключ или пароль в диалоге SFTP.",
                    "SSH agent ({error}). Add a key to the agent or supply a key or password in SFTP."
                )
            }),
        }
    }
}

fn auth_methods(session: &Session, user: &str) -> Result<String, String> {
    match session.auth_methods(user) {
        Ok(methods) => Ok(methods.to_owned()),
        Err(_) if session.authenticated() => Ok(String::new()),
        Err(error) => Err(err(error)),
    }
}

fn offers(methods: &str, method: &str) -> bool {
    methods.split(',').any(|value| value == method)
}

fn authenticate(
    session: &Session,
    user: &str,
    primary: &PrimaryAuth<'_>,
    interactive: &mut InteractiveAuth,
) -> Result<bool, String> {
    let control = interactive.control();
    let mut methods = auth_methods(session, user)?;
    let mut tried_primary = false;
    let mut tried_interactive = false;
    let mut key_accepted = false;
    let mut failure = None;
    while !session.authenticated() {
        control.check()?;
        if !tried_primary && offers(&methods, primary.method()) {
            tried_primary = true;
            failure = primary.attempt(session, user).err();
            if session.authenticated() {
                return Ok(primary.is_explicit_key());
            }
            control.check()?;
            methods = auth_methods(session, user)?;
            // libssh2 reports a partial login as failure. A server that moves
            // from the key to keyboard-interactive has accepted the key factor.
            key_accepted = primary.is_explicit_key()
                && !offers(&methods, primary.method())
                && offers(&methods, "keyboard-interactive");
        } else if !tried_interactive && offers(&methods, "keyboard-interactive") {
            tried_interactive = true;
            interactive.authenticate(session, user);
            control.check()?;
            if !session.authenticated() {
                failure = Some(text("Сервер не подтвердил интерактивную аутентификацию").into());
                if tried_primary {
                    break;
                }
                methods = auth_methods(session, user)?;
            }
        } else {
            break;
        }
    }
    if session.authenticated() {
        Ok(key_accepted)
    } else {
        Err(failure
            .unwrap_or_else(|| text("Сервер не предложил доступный способ аутентификации").into()))
    }
}

pub(crate) fn connect_location(
    request: ConnectRequest,
    auth: InteractiveAuth,
) -> Result<ConnectedLocation, String> {
    let known_hosts = dirs::home_dir()
        .ok_or("Не найдена домашняя папка")?
        .join(".ssh/known_hosts");
    connect_location_at(request, auth, &known_hosts)
}

pub(super) fn connect_location_at(
    request: ConnectRequest,
    mut auth: InteractiveAuth,
    known_hosts: &Path,
) -> Result<ConnectedLocation, String> {
    let control = auth.control();
    let result = connect_reviewed(request, &mut auth, &control, known_hosts);
    control.finalize()?;
    result
}

fn connect_reviewed(
    request: ConnectRequest,
    auth: &mut InteractiveAuth,
    control: &ConnectionControl,
    known_hosts: &Path,
) -> Result<ConnectedLocation, String> {
    let ConnectRequest {
        endpoint,
        path,
        password,
        passphrase,
        use_saved,
        save_key,
    } = request;
    control.check()?;
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
    let session = open_verified(&endpoint, known_hosts, Some(control))?;
    let used_key = authenticate(
        &session,
        &endpoint.user,
        &PrimaryAuth::new(&endpoint, &password, phrase, key_data),
        auth,
    )?;
    let sftp = session.sftp().map_err(err)?;
    let path = sftp
        .realpath(Path::new(if path.is_empty() { "." } else { &path }))
        .map_err(err)?;
    if !sftp.stat(&path).map_err(err)?.is_dir() {
        return Err("Начальный путь не является папкой".into());
    }
    control.finalize()?;
    let saved_profile = save_key && used_key;
    if saved_profile {
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
    Ok(ConnectedLocation {
        location: Location::Sftp { endpoint, path },
        saved_profile,
    })
}

pub(super) fn remote(location: &Location) -> Result<Sftp, String> {
    let Location::Sftp { endpoint, .. } = location else {
        if location.is_archive() {
            return Err(archive::readonly().into());
        }
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
    session.sftp().map_err(|e| {
        crate::tr_format!(
            "SFTP: {e}. Для повторного подключения используйте кнопку SFTP.",
            "SFTP: {e}. Use the SFTP button to reconnect."
        )
    })
}
