use super::{Endpoint, InteractiveAuth};
use crate::{i18n::text, name_match::Mask};
use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ssh2::{CheckResult, KnownHostFileKind, KnownHosts, MethodType, Session};
use std::{
    fmt::Write as _,
    fs::{self, File},
    io::{BufRead, BufReader, ErrorKind, Read, Write},
    net::IpAddr,
    path::{Path, PathBuf},
};

pub(crate) struct HostKey {
    pub address: String,
    pub algorithm: String,
    pub fingerprint: String,
}

pub(in crate::vfs) struct TrustFiles {
    known_hosts: PathBuf,
    pins: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Pin {
    address: String,
    fingerprint: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Lookup {
    Match,
    Mismatch,
    Missing,
}

struct HostNames<'a> {
    original: &'a str,
    host: String,
    port: u16,
    scoped: String,
}

impl<'a> HostNames<'a> {
    fn new(endpoint: &'a Endpoint) -> Self {
        let original = endpoint.host_name();
        let host = original.parse::<IpAddr>().map_or_else(
            |_| {
                if original.contains(':') {
                    original.to_owned()
                } else {
                    original.to_ascii_lowercase()
                }
            },
            |address| address.to_string(),
        );
        let port = endpoint.port;
        Self {
            original,
            scoped: format!("[{host}]:{port}"),
            host,
            port,
        }
    }
}

impl TrustFiles {
    pub(super) fn user() -> Result<Self, String> {
        #[cfg(test)]
        if std::env::var_os("PAWCOMMANDER_TEST_SFTP_RECONNECT").as_deref()
            == Some(std::ffi::OsStr::new("1"))
            && let Some(root) = std::env::var_os("PAWCOMMANDER_TEST_SFTP_ROOT")
        {
            return Ok(Self::new(PathBuf::from(root).join("known_hosts"), None));
        }
        let home = dirs::home_dir().ok_or_else(|| text("Не найдена домашняя папка"))?;
        Ok(Self::new(
            home.join(".ssh/known_hosts"),
            dirs::data_local_dir().map(|root| root.join("PawCommander").join("ssh-host-keys")),
        ))
    }

    pub(in crate::vfs) fn new(known_hosts: PathBuf, pins: Option<PathBuf>) -> Self {
        Self { known_hosts, pins }
    }

    pub(super) fn verify(
        &self,
        session: &Session,
        endpoint: &Endpoint,
        interactive: Option<&InteractiveAuth>,
    ) -> Result<(), String> {
        let (key, _) = session
            .host_key()
            .ok_or_else(|| text("Сервер не предоставил ключ SSH"))?;
        let names = HostNames::new(endpoint);
        let pin = Pin {
            address: names.scoped.clone(),
            fingerprint: fingerprint(key),
        };
        let path = self.pins.as_ref().map(|dir| pin_path(dir, &pin.address));
        let trusted = || self.trusted(session, &names, key, &pin, path.as_deref());
        if trusted()? {
            return Ok(());
        }
        let Some(interactive) = interactive else {
            return Err(text(
                "Ключ сервера SSH не подтверждён. Откройте форму SFTP и проверьте отпечаток.",
            )
            .into());
        };
        let path = path
            .as_deref()
            .ok_or_else(|| text("Не найдена папка для доверенных ключей PawCommander"))?;
        interactive.confirm_host_key(HostKey {
            address: pin.address.clone(),
            algorithm: session.methods(MethodType::HostKey).unwrap_or("SSH").into(),
            fingerprint: pin.fingerprint.clone(),
        })?;
        // Another process may have changed either trust file while the dialog was open.
        if !trusted()? {
            remember(path, &pin)?;
        }
        Ok(())
    }

    fn trusted(
        &self,
        session: &Session,
        names: &HostNames<'_>,
        key: &[u8],
        pin: &Pin,
        path: Option<&Path>,
    ) -> Result<bool, String> {
        let known = check_known_hosts(session, &self.known_hosts, names, key).map_err(|error| {
            let path = self.known_hosts.display();
            crate::tr_format!(
                "Не удалось проверить ключ SSH по {path} ({error})",
                "Could not verify the SSH host key using {path} ({error})"
            )
        })?;
        if known == Lookup::Mismatch {
            let changed = text("Ключ сервера SSH изменился.");
            let fingerprint = &pin.fingerprint;
            let path = self.known_hosts.display();
            return Err(crate::tr_format!(
                "{changed} Сервер предоставил {fingerprint}. Сверьте отпечаток у администратора и проверьте запись в {path}.",
                "{changed} The server presented {fingerprint}. Verify the fingerprint with the administrator and review the entry in {path}."
            ));
        }
        let saved = match path {
            Some(path) => check_pin(path, pin)?,
            None => false,
        };
        Ok(known == Lookup::Match || saved)
    }
}

fn fingerprint(key: &[u8]) -> String {
    format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(key)))
}

fn check_known_hosts(
    session: &Session,
    path: &Path,
    names: &HostNames<'_>,
    key: &[u8],
) -> Result<Lookup, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Lookup::Missing),
        Err(error) => return Err(error.to_string()),
    };
    let mut known = session.known_hosts().map_err(|error| error.to_string())?;
    let mut revoked = session.known_hosts().map_err(|error| error.to_string())?;
    let mut authorities = session.known_hosts().map_err(|error| error.to_string())?;
    for line in BufReader::new(file).split(b'\n') {
        let line = line.map_err(|error| error.to_string())?;
        read_line(&line, names, &mut known, &mut revoked, &mut authorities)?;
    }
    let original_scoped = format!("[{}]:{}", names.original, names.port);
    let lookup = |hosts: &KnownHosts, label: &str| match hosts.check(label, key) {
        CheckResult::Match => Ok(Lookup::Match),
        CheckResult::Mismatch => Ok(Lookup::Mismatch),
        CheckResult::NotFound => Ok(Lookup::Missing),
        CheckResult::Failure => Err(text("Ошибка проверки записи known_hosts").to_owned()),
    };
    let check =
        |hosts: &KnownHosts, canonical: &str, original: &str| match lookup(hosts, canonical)? {
            Lookup::Missing if canonical != original => lookup(hosts, original),
            result => Ok(result),
        };
    let scoped = |hosts| check(hosts, &names.scoped, &original_scoped);
    let bare = |hosts| {
        if names.port == 22 {
            check(hosts, &names.host, names.original)
        } else {
            Ok(Lookup::Missing)
        }
    };
    let revoked_at = |canonical: &str, original: &str| {
        Ok::<_, String>(
            lookup(&revoked, canonical)? == Lookup::Match
                || (canonical != original && lookup(&revoked, original)? == Lookup::Match),
        )
    };
    if revoked_at(&names.scoped, &original_scoped)?
        || (names.port == 22 && revoked_at(&names.host, names.original)?)
    {
        return Err(text("Ключ сервера SSH отозван").into());
    }
    let result = match scoped(&known)? {
        Lookup::Missing => bare(&known)?,
        result => result,
    };
    if result == Lookup::Missing
        && (scoped(&authorities)? != Lookup::Missing || bare(&authorities)? != Lookup::Missing)
    {
        return Err(text("Для этого сервера в known_hosts указан центр сертификации SSH. Проверьте сертификат через OpenSSH.").into());
    }
    Ok(result)
}

fn read_line(
    line: &[u8],
    names: &HostNames<'_>,
    known: &mut KnownHosts,
    revoked: &mut KnownHosts,
    authorities: &mut KnownHosts,
) -> Result<(), String> {
    let invalid = || text("Некорректная запись known_hosts").to_owned();
    let mut fields = line
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    let Some(first) = fields.next() else {
        return Ok(());
    };
    if first.starts_with(b"#") {
        return Ok(());
    }
    let (entries, hosts) = match first {
        b"@revoked" => (revoked, fields.next().ok_or_else(invalid)?),
        b"@cert-authority" => (authorities, fields.next().ok_or_else(invalid)?),
        marker if marker.starts_with(b"@") => return Err(invalid()),
        _ => (known, first),
    };
    let hosts = std::str::from_utf8(hosts).map_err(|_| invalid())?;
    let algorithm =
        std::str::from_utf8(fields.next().ok_or_else(invalid)?).map_err(|_| invalid())?;
    let key = std::str::from_utf8(fields.next().ok_or_else(invalid)?).map_err(|_| invalid())?;
    let mut add = |host: &str| {
        entries
            .read_str(
                &format!("{host} {algorithm} {key}"),
                KnownHostFileKind::OpenSSH,
            )
            .map_err(|error| error.to_string())
    };
    if hosts.starts_with('|') {
        add(hosts)?;
    } else {
        let hosts = hosts.to_ascii_lowercase();
        if hosts.contains(['*', '?', '!']) {
            for name in [&names.scoped, &names.host] {
                if matches_pattern_list(&hosts, name) {
                    add(name)?;
                }
            }
        } else {
            add(&hosts)?;
        }
    }
    Ok(())
}

fn matches_pattern_list(patterns: &str, host: &str) -> bool {
    let mut matched = false;
    for pattern in patterns.split(',') {
        let negative = pattern.strip_prefix('!');
        if Mask::new(negative.unwrap_or(pattern)).matches(host) {
            if negative.is_some() {
                return false;
            }
            matched = true;
        }
    }
    matched
}

fn pin_path(directory: &Path, address: &str) -> PathBuf {
    let mut name = String::with_capacity(69);
    for byte in Sha256::digest(address.as_bytes()) {
        write!(name, "{byte:02x}").expect("writing to String succeeds");
    }
    name.push_str(".json");
    directory.join(name)
}

fn pin_io_error(path: &Path, error: impl std::fmt::Display) -> String {
    let path = path.display();
    crate::tr_format!(
        "Не удалось прочитать или сохранить доверенный ключ SSH в {path} ({error})",
        "Could not read or save the trusted SSH key in {path} ({error})"
    )
}

fn check_pin(path: &Path, expected: &Pin) -> Result<bool, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(pin_io_error(path, error)),
    };
    let mut bytes = Vec::new();
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(|error| pin_io_error(path, error))?;
    if bytes.len() > 4096 {
        return Err(pin_io_error(
            path,
            text("Запись доверенного ключа слишком велика"),
        ));
    }
    let saved: Pin = serde_json::from_slice(&bytes).map_err(|error| pin_io_error(path, error))?;
    if saved != *expected {
        let changed = text("Ключ сервера SSH изменился.");
        let path = path.display();
        let received = &expected.fingerprint;
        let saved = &saved.fingerprint;
        return Err(crate::tr_format!(
            "{changed} Сохранён {saved}, получен {received}. Сверьте отпечаток у администратора и проверьте запись в {path}.",
            "{changed} Saved fingerprint {saved}, received {received}. Verify the fingerprint with the administrator and review the record in {path}."
        ));
    }
    Ok(true)
}

fn remember(path: &Path, pin: &Pin) -> Result<(), String> {
    let directory = path.parent().expect("pin has a parent directory");
    let stage = || -> std::io::Result<tempfile::NamedTempFile> {
        fs::create_dir_all(directory)?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut file, pin)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        Ok(file)
    };
    match stage()
        .map_err(|error| pin_io_error(path, error))?
        .persist_noclobber(path)
    {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists && check_pin(path, pin)? => {
            Ok(())
        }
        Err(error) => Err(pin_io_error(path, error.error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    #[test]
    fn ssh_known_hosts_keeps_ports_distinct_and_honors_revocation() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("known_hosts");
        let session = Session::new().unwrap();
        let mut endpoint = Endpoint {
            host: "Fixture.Example".into(),
            port: 2222,
            user: "account".into(),
            key: None,
            jump: None,
        };
        let old = b"fixture-ssh-host-key-one";
        let new = b"fixture-ssh-host-key-two";
        let old_encoded = STANDARD.encode(old);
        let new_encoded = STANDARD.encode(new);
        let entries = format!(
            "fixture.example ssh-ed25519 {old_encoded}\n[fixture.example]:2222 ssh-ed25519 {new_encoded}\n"
        );
        fs::write(&path, &entries).unwrap();
        let check = |endpoint: &Endpoint, key: &[u8]| {
            check_known_hosts(&session, &path, &HostNames::new(endpoint), key)
        };
        assert_eq!(check(&endpoint, old).unwrap(), Lookup::Mismatch);
        assert_eq!(check(&endpoint, new).unwrap(), Lookup::Match);
        endpoint.port = 22;
        assert_eq!(check(&endpoint, old).unwrap(), Lookup::Match);
        endpoint.port = 2223;
        assert_eq!(check(&endpoint, old).unwrap(), Lookup::Missing);
        endpoint.port = 2222;
        fs::write(
            &path,
            format!(
                "{entries}@revoked [*.example]:2222,![another.example]:2222 ssh-ed25519 {new_encoded}\n"
            ),
        )
        .unwrap();
        assert_eq!(
            check(&endpoint, new).unwrap_err(),
            text("Ключ сервера SSH отозван")
        );
        fs::write(
            &path,
            format!("@cert-authority [*.example]:2222 ssh-ed25519 {old_encoded}\n"),
        )
        .unwrap();
        assert_eq!(
            check(&endpoint, new).unwrap_err(),
            text("Для этого сервера в known_hosts указан центр сертификации SSH. Проверьте сертификат через OpenSSH.")
        );
    }

    #[test]
    fn ssh_host_pin_persists_without_replacing_a_competing_key() {
        let dir = tempfile::tempdir().unwrap();
        let pinned = Pin {
            address: "[host.example]:22".into(),
            fingerprint: fingerprint(b"confirmed fixture key"),
        };
        let path = pin_path(dir.path(), &pinned.address);
        remember(&path, &pinned).unwrap();
        let original = fs::read(&path).unwrap();
        remember(&path, &pinned).unwrap();
        let changed = Pin {
            address: pinned.address.clone(),
            fingerprint: fingerprint(b"other fixture key"),
        };
        assert!(remember(&path, &changed)
            .unwrap_err()
            .starts_with(text("Ключ сервера SSH изменился.")));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!check_pin(&pin_path(dir.path(), "[host.example]:2222"), &pinned).unwrap());
    }
}
