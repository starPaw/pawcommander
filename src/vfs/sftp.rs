use super::err;
use super::location::{Endpoint, Location};
use crate::archive;
use ssh2::{CheckResult, KnownHostFileKind, Session, Sftp};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

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
