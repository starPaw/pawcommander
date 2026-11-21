use super::{Endpoint, InteractiveAuth};
use crate::file_error::{HostKeyError, KnownHostEntryError, KnownHostsError, PinError};
use crate::name_match::Mask;
use crate::operations::TransferError;
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
    pub(super) fn user() -> Result<Self, HostKeyError> {
        #[cfg(test)]
        if std::env::var_os("PAWCOMMANDER_TEST_SFTP_RECONNECT").as_deref()
            == Some(std::ffi::OsStr::new("1"))
            && let Some(root) = std::env::var_os("PAWCOMMANDER_TEST_SFTP_ROOT")
        {
            return Ok(Self::new(PathBuf::from(root).join("known_hosts"), None));
        }
        let home = dirs::home_dir().ok_or(HostKeyError::HomeUnavailable)?;
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
    ) -> Result<(), TransferError> {
        let (key, _) = session.host_key().ok_or(HostKeyError::KeyUnavailable)?;
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
            return Err(HostKeyError::Unverified.into());
        };
        let path = path
            .as_deref()
            .ok_or(HostKeyError::PinDirectoryUnavailable)?;
        interactive.confirm_host_key(HostKey {
            address: pin.address.clone(),
            algorithm: session.methods(MethodType::HostKey).unwrap_or("SSH").into(),
            fingerprint: pin.fingerprint.clone(),
        })?;
        // Another process may have changed either trust file while the dialog was open.
        if !trusted()? {
            remember(path, &pin).map_err(|cause| HostKeyError::Pin {
                path: path.to_owned(),
                cause,
            })?;
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
    ) -> Result<bool, HostKeyError> {
        let known = check_known_hosts(session, &self.known_hosts, names, key).map_err(|cause| {
            HostKeyError::KnownHosts {
                path: self.known_hosts.clone(),
                cause,
            }
        })?;
        if known == Lookup::Mismatch {
            return Err(HostKeyError::KnownHostsChanged {
                path: self.known_hosts.clone(),
                received: pin.fingerprint.clone(),
            });
        }
        let saved = match path {
            Some(path) => check_pin(path, pin).map_err(|cause| HostKeyError::Pin {
                path: path.to_owned(),
                cause,
            })?,
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
) -> Result<Lookup, KnownHostsError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Lookup::Missing),
        Err(error) => return Err(KnownHostsError::Read(error)),
    };
    let mut known = session.known_hosts().map_err(KnownHostsError::Initialize)?;
    let mut revoked = session.known_hosts().map_err(KnownHostsError::Initialize)?;
    let mut authorities = session.known_hosts().map_err(KnownHostsError::Initialize)?;
    for (index, line) in BufReader::new(file).split(b'\n').enumerate() {
        let line = line.map_err(KnownHostsError::Read)?;
        read_line(&line, names, &mut known, &mut revoked, &mut authorities).map_err(|cause| {
            KnownHostsError::Entry {
                line: index + 1,
                cause,
            }
        })?;
    }
    let original_scoped = format!("[{}]:{}", names.original, names.port);
    let lookup = |hosts: &KnownHosts, label: &str| match hosts.check(label, key) {
        CheckResult::Match => Ok(Lookup::Match),
        CheckResult::Mismatch => Ok(Lookup::Mismatch),
        CheckResult::NotFound => Ok(Lookup::Missing),
        CheckResult::Failure => Err(KnownHostsError::Check),
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
        Ok::<_, KnownHostsError>(
            lookup(&revoked, canonical)? == Lookup::Match
                || (canonical != original && lookup(&revoked, original)? == Lookup::Match),
        )
    };
    if revoked_at(&names.scoped, &original_scoped)?
        || (names.port == 22 && revoked_at(&names.host, names.original)?)
    {
        return Err(KnownHostsError::Revoked);
    }
    let result = match scoped(&known)? {
        Lookup::Missing => bare(&known)?,
        result => result,
    };
    if result == Lookup::Missing
        && (scoped(&authorities)? != Lookup::Missing || bare(&authorities)? != Lookup::Missing)
    {
        return Err(KnownHostsError::CertificateAuthority);
    }
    Ok(result)
}

fn read_line(
    line: &[u8],
    names: &HostNames<'_>,
    known: &mut KnownHosts,
    revoked: &mut KnownHosts,
    authorities: &mut KnownHosts,
) -> Result<(), KnownHostEntryError> {
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
        b"@revoked" => (revoked, fields.next().ok_or(KnownHostEntryError::Format)?),
        b"@cert-authority" => (
            authorities,
            fields.next().ok_or(KnownHostEntryError::Format)?,
        ),
        marker if marker.starts_with(b"@") => return Err(KnownHostEntryError::Format),
        _ => (known, first),
    };
    let hosts = std::str::from_utf8(hosts).map_err(KnownHostEntryError::Encoding)?;
    let mut next_field = || {
        std::str::from_utf8(fields.next().ok_or(KnownHostEntryError::Format)?)
            .map_err(KnownHostEntryError::Encoding)
    };
    let algorithm = next_field()?;
    let key = next_field()?;
    let mut add = |host: &str| {
        entries
            .read_str(
                &format!("{host} {algorithm} {key}"),
                KnownHostFileKind::OpenSSH,
            )
            .map_err(KnownHostEntryError::Parse)
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

fn check_pin(path: &Path, expected: &Pin) -> Result<bool, PinError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(PinError::Read(error)),
    };
    let mut bytes = Vec::new();
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(PinError::Read)?;
    if bytes.len() > 4096 {
        return Err(PinError::TooLarge);
    }
    let saved: Pin = serde_json::from_slice(&bytes).map_err(PinError::Json)?;
    if saved != *expected {
        return Err(PinError::Changed {
            saved: saved.fingerprint,
            received: expected.fingerprint.clone(),
        });
    }
    Ok(true)
}

fn remember(path: &Path, pin: &Pin) -> Result<(), PinError> {
    let directory = path.parent().expect("pin has a parent directory");
    fs::create_dir_all(directory).map_err(PinError::Write)?;
    let mut file = tempfile::NamedTempFile::new_in(directory).map_err(PinError::Write)?;
    serde_json::to_writer(&mut file, pin).map_err(PinError::Json)?;
    file.write_all(b"\n").map_err(PinError::Write)?;
    file.as_file().sync_all().map_err(PinError::Write)?;
    match file.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists && check_pin(path, pin)? => {
            Ok(())
        }
        Err(error) => Err(PinError::Write(error.error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use std::error::Error as _;

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
        assert!(matches!(
            check(&endpoint, new),
            Err(KnownHostsError::Revoked)
        ));
        fs::write(
            &path,
            format!("@cert-authority [*.example]:2222 ssh-ed25519 {old_encoded}\n"),
        )
        .unwrap();
        assert!(matches!(
            check(&endpoint, new),
            Err(KnownHostsError::CertificateAuthority)
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn invalid_known_hosts_blocks_a_matching_pin_and_preserves_the_file_and_cause() {
        let home = tempfile::tempdir().unwrap();
        let known = home.path().join("known_hosts");
        let directory = home.path().join("pins");
        let trust = TrustFiles::new(known.clone(), Some(directory.clone()));
        let session = Session::new().unwrap();
        let endpoint = Endpoint {
            host: "fixture.example".into(),
            port: 22,
            user: "account".into(),
            key: None,
            jump: None,
        };
        let names = HostNames::new(&endpoint);
        let key = b"confirmed fixture key";
        let pin = Pin {
            address: names.scoped.clone(),
            fingerprint: fingerprint(key),
        };
        let path = pin_path(&directory, &pin.address);
        remember(&path, &pin).unwrap();
        let pinned = fs::read(&path).unwrap();
        let check = || trust.trusted(&session, &names, key, &pin, Some(&path));
        assert!(check().unwrap());

        let invalid = b"# fixture comment\nfixture.example ssh-ed25519 \xff\n";
        fs::write(&known, invalid).unwrap();
        let error = check().unwrap_err();
        assert!(matches!(&error, HostKeyError::KnownHosts {
            path,
            cause: KnownHostsError::Entry { line: 2, cause: KnownHostEntryError::Encoding(_) },
        } if path == &known));
        let encoding = error.source().unwrap().source().unwrap().source().unwrap();
        assert!(encoding.is::<std::str::Utf8Error>());
        assert_eq!(fs::read(&known).unwrap(), invalid);
        assert_eq!(fs::read(&path).unwrap(), pinned);

        fs::remove_file(&known).unwrap();
        fs::create_dir(&known).unwrap();
        assert!(matches!(check(), Err(HostKeyError::KnownHosts {
            path,
            cause: KnownHostsError::Read(source),
        }) if path == known && source.raw_os_error() == Some(libc::EISDIR)));
        assert_eq!(fs::read(&path).unwrap(), pinned);
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
        assert!(
            matches!(remember(&path, &changed), Err(PinError::Changed { saved, received })
            if saved == pinned.fingerprint && received == changed.fingerprint)
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!check_pin(&pin_path(dir.path(), "[host.example]:2222"), &pinned).unwrap());
    }

    #[test]
    fn malformed_and_oversized_pins_preserve_the_existing_record() {
        let dir = tempfile::tempdir().unwrap();
        let pin = Pin {
            address: "[host.example]:22".into(),
            fingerprint: fingerprint(b"confirmed fixture key"),
        };
        let path = pin_path(dir.path(), &pin.address);
        let malformed = b"{\n";
        fs::write(&path, malformed).unwrap();
        let error = remember(&path, &pin).unwrap_err();
        assert!(matches!(&error, PinError::Json(source) if source.is_eof()));
        assert!(error.source().unwrap().is::<serde_json::Error>());
        assert_eq!(fs::read(&path).unwrap(), malformed);

        let oversized = vec![b' '; 4097];
        fs::write(&path, &oversized).unwrap();
        assert!(matches!(check_pin(&path, &pin), Err(PinError::TooLarge)));
        assert_eq!(fs::read(&path).unwrap(), oversized);
    }
}
