use super::err;
use super::location::{Endpoint, Location};
use crate::file_error::{
    AccessAction, AccessError, ConnectionError, MutationError, ScanError, SshStage,
};
use crate::operations::TransferError;
use libssh2_sys::{
    LIBSSH2_ERROR_CHANNEL_CLOSED, LIBSSH2_ERROR_CHANNEL_FAILURE, LIBSSH2_ERROR_SOCKET_DISCONNECT,
    LIBSSH2_ERROR_SOCKET_RECV, LIBSSH2_ERROR_SOCKET_SEND, LIBSSH2_ERROR_SOCKET_TIMEOUT,
    LIBSSH2_ERROR_TIMEOUT, LIBSSH2_FX_CONNECTION_LOST, LIBSSH2_FX_NO_CONNECTION,
};
use ssh2::{ErrorCode, FileType, Session, Sftp};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use zeroize::Zeroizing;

mod host_key;
mod interactive;
#[cfg(target_os = "macos")]
mod proxy_jump;
#[cfg(test)]
pub(crate) use host_key::HostKey;
pub(super) use host_key::TrustFiles;
pub(crate) use interactive::{
    AuthRequest, ConnectionControl, ConnectionPrompt, HostKeyRequest, HostKeyRole, InteractiveAuth,
};

const IO_TIMEOUT_MS: u32 = 20_000;
const MAX_IMPORTED_KEY_BYTES: usize = 64 * 1024;

pub(crate) struct ConnectRequest {
    pub endpoint: Endpoint,
    pub path: String,
    pub password: Zeroizing<String>,
    pub passphrase: Zeroizing<String>,
    #[cfg(target_os = "macos")]
    pub jump_passphrase: Zeroizing<String>,
    pub use_saved: bool,
    pub save_key: bool,
}

pub(crate) struct ConnectedLocation {
    pub location: Location,
    pub saved_profile: bool,
}

static SESSIONS: OnceLock<Mutex<HashMap<Endpoint, Arc<Session>>>> = OnceLock::new();
type OpeningGate = Mutex<()>;
static OPENING: OnceLock<Mutex<HashMap<Endpoint, Weak<OpeningGate>>>> = OnceLock::new();

pub(super) fn sessions() -> &'static Mutex<HashMap<Endpoint, Arc<Session>>> {
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

#[cfg(test)]
pub(super) fn connect_with_key(
    endpoint: &Endpoint,
    password: &str,
    passphrase: &str,
    known_hosts: &Path,
    private_key: Option<&str>,
) -> Result<Session, String> {
    let trust = TrustFiles::new(known_hosts.to_owned(), None);
    let session = open_connection(
        endpoint,
        &trust,
        None,
        #[cfg(target_os = "macos")]
        proxy_jump::Login {
            use_saved: false,
            passphrase: "",
        },
    )
    .map_err(|error| crate::i18n::connection_result_error(&error))?;
    login_primary(
        &session,
        &endpoint.user,
        PrimaryAuth::new(endpoint, password, passphrase, private_key),
    )
    .map_err(|error| crate::i18n::connection_result_error(&error))?;
    Ok(session)
}

fn login_primary(
    session: &Session,
    user: &str,
    primary: PrimaryAuth<'_>,
) -> Result<(), TransferError> {
    primary.attempt(session, user)?;
    if !session.authenticated() {
        return Err(ConnectionError::AuthenticationUnconfirmed { interactive: false }.into());
    }
    Ok(())
}

fn open_verified(
    endpoint: &Endpoint,
    trust: &TrustFiles,
    interactive: Option<&InteractiveAuth>,
) -> Result<Session, TransferError> {
    validate_endpoint(endpoint)?;
    let host = endpoint.host_name();
    // Bound TCP and SSH I/O waits. DNS resolution uses the OS resolver.
    let control = interactive.map(InteractiveAuth::control);
    let control = control.as_ref();
    let addresses =
        (host, endpoint.port)
            .to_socket_addrs()
            .map_err(|source| ConnectionError::Resolve {
                host: host.to_owned(),
                source,
            })?;
    let mut tcp = None;
    let mut last = None;
    for address in addresses {
        if let Some(control) = control {
            control.check()?;
        }
        match TcpStream::connect_timeout(&address, Duration::from_secs(10)) {
            Ok(stream) => {
                tcp = Some(stream);
                break;
            }
            Err(error) => last = Some((address, error)),
        }
    }
    let tcp = tcp.ok_or_else(|| ConnectionError::Connect {
        endpoint: endpoint.label(),
        last,
    })?;
    if let Some(control) = control {
        let socket = tcp.try_clone().map_err(ConnectionError::Transport)?;
        control.attach(move || {
            let _ = socket.shutdown(Shutdown::Both);
        })?;
    }
    let timeout = Some(Duration::from_millis(u64::from(IO_TIMEOUT_MS)));
    tcp.set_read_timeout(timeout)
        .map_err(ConnectionError::Transport)?;
    tcp.set_write_timeout(timeout)
        .map_err(ConnectionError::Transport)?;
    let mut session = Session::new().map_err(ssh_failure(SshStage::Session))?;
    session.set_tcp_stream(tcp);
    verify_session(session, endpoint, trust, interactive)
}

fn validate_endpoint(endpoint: &Endpoint) -> Result<(), TransferError> {
    let host = endpoint.host_name();
    if host.is_empty()
        || host.chars().any(|ch| ch.is_control() || ch.is_whitespace())
        || endpoint.user.trim().is_empty()
        || endpoint.user.chars().any(char::is_control)
        || endpoint.port == 0
    {
        return Err(ConnectionError::InvalidEndpoint.into());
    }
    Ok(())
}

fn verify_session(
    mut session: Session,
    endpoint: &Endpoint,
    trust: &TrustFiles,
    interactive: Option<&InteractiveAuth>,
) -> Result<Session, TransferError> {
    session.set_timeout(IO_TIMEOUT_MS);
    session
        .handshake()
        .map_err(ssh_failure(SshStage::Handshake))?;
    trust.verify(&session, endpoint, interactive)?;
    Ok(session)
}

fn open_connection(
    endpoint: &Endpoint,
    trust: &TrustFiles,
    interactive: Option<&mut InteractiveAuth>,
    #[cfg(target_os = "macos")] login: proxy_jump::Login<'_>,
) -> Result<Session, TransferError> {
    if let Some(jump) = &endpoint.jump {
        #[cfg(target_os = "macos")]
        return proxy_jump::open(endpoint, jump, trust, interactive, login);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = jump;
            return Err(crate::i18n::text!("Подключения через ProxyJump доступны на macOS").into());
        }
    }
    open_verified(endpoint, trust, interactive.as_deref())
}

fn ssh_failure(stage: SshStage) -> impl FnOnce(ssh2::Error) -> TransferError {
    move |source| ConnectionError::Ssh { stage, source }.into()
}

fn route_error(address: String, jump: bool, cause: TransferError) -> TransferError {
    if matches!(cause, TransferError::Cancelled) {
        cause
    } else {
        ConnectionError::Route {
            jump,
            address,
            cause,
        }
        .into()
    }
}

fn at_target(endpoint: &Endpoint, error: TransferError) -> TransferError {
    if endpoint.jump.is_some() {
        route_error(endpoint.label(), false, error)
    } else {
        error
    }
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

    fn attempt(&self, session: &Session, user: &str) -> Result<(), TransferError> {
        let failure = ssh_failure(SshStage::Authentication);
        match *self {
            Self::Password(password) => session.userauth_password(user, password).map_err(failure),
            Self::MemoryKey(key, phrase) => session
                .userauth_pubkey_memory(user, None, key, phrase)
                .map_err(failure),
            Self::KeyFile(key, phrase) => session
                .userauth_pubkey_file(user, None, key, phrase)
                .map_err(failure),
            Self::Agent => session
                .userauth_agent(user)
                .map_err(ssh_failure(SshStage::Agent)),
        }
    }
}

fn auth_methods(session: &Session, user: &str) -> Result<String, TransferError> {
    match session.auth_methods(user) {
        Ok(methods) => Ok(methods.to_owned()),
        Err(_) if session.authenticated() => Ok(String::new()),
        Err(error) => Err(ssh_failure(SshStage::AuthenticationMethods)(error)),
    }
}

fn offers(methods: &str, method: &str) -> bool {
    methods.split(',').any(|value| value == method)
}

fn authenticate(
    session: &Session,
    endpoint: &Endpoint,
    primary: &PrimaryAuth<'_>,
    interactive: &mut InteractiveAuth,
) -> Result<bool, TransferError> {
    let user = &endpoint.user;
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
            let result = interactive.authenticate(session, endpoint);
            control.check()?;
            if !session.authenticated() {
                failure = Some(result.err().map_or_else(
                    || ConnectionError::AuthenticationUnconfirmed { interactive: true }.into(),
                    ssh_failure(SshStage::InteractiveAuthentication),
                ));
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
        Err(failure.unwrap_or_else(|| ConnectionError::UnsupportedAuthentication.into()))
    }
}

pub(crate) fn connect_location(
    request: ConnectRequest,
    auth: InteractiveAuth,
) -> Result<ConnectedLocation, TransferError> {
    connect_location_at(request, auth, &TrustFiles::user()?)
}

pub(super) fn connect_location_at(
    request: ConnectRequest,
    mut auth: InteractiveAuth,
    trust: &TrustFiles,
) -> Result<ConnectedLocation, TransferError> {
    let control = auth.control();
    let result = connect_reviewed(request, &mut auth, &control, trust);
    control.finalize()?;
    result
}

fn read_key_to_save(
    endpoint: &Endpoint,
    password: &str,
) -> Result<Zeroizing<String>, TransferError> {
    if !password.is_empty() {
        return Err(ConnectionError::KeyPasswordConflict.into());
    }
    let path = endpoint
        .key
        .as_ref()
        .ok_or(ConnectionError::KeyFileRequired)?;
    let location = Location::Local(path.clone());
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let input = options
        .open(path)
        .map_err(|source| AccessError::new(&location, AccessAction::OpenFile, source))?;
    let metadata = input
        .metadata()
        .map_err(|source| AccessError::new(&location, AccessAction::ReadOpenMetadata, source))?;
    if !metadata.is_file() {
        return Err(ConnectionError::ImportedKeyNotFile(path.clone()).into());
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_IMPORTED_KEY_BYTES + 1));
    input
        .take((MAX_IMPORTED_KEY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| AccessError::new(&location, AccessAction::ReadFile, source))?;
    if bytes.len() > MAX_IMPORTED_KEY_BYTES {
        return Err(ConnectionError::ImportedKeyTooLarge {
            path: path.clone(),
            limit: MAX_IMPORTED_KEY_BYTES,
        }
        .into());
    }
    // Utf8Error retains the byte offset without retaining key material.
    let text =
        std::str::from_utf8(&bytes).map_err(|source| ConnectionError::ImportedKeyEncoding {
            path: path.clone(),
            source,
        })?;
    Ok(Zeroizing::new(text.to_owned()))
}

fn connect_reviewed(
    request: ConnectRequest,
    auth: &mut InteractiveAuth,
    control: &ConnectionControl,
    trust: &TrustFiles,
) -> Result<ConnectedLocation, TransferError> {
    let ConnectRequest {
        endpoint,
        path,
        password,
        passphrase,
        #[cfg(target_os = "macos")]
        jump_passphrase,
        use_saved,
        save_key,
    } = request;
    control.check()?;
    validate_endpoint(&endpoint).map_err(|error| at_target(&endpoint, error))?;
    let imported = if save_key {
        Some(
            read_key_to_save(&endpoint, &password)
                .map_err(|error| route_error(endpoint.label(), false, error))?,
        )
    } else {
        None
    };
    control.check()?;
    let session = open_connection(
        &endpoint,
        trust,
        Some(auth),
        #[cfg(target_os = "macos")]
        proxy_jump::Login {
            use_saved,
            passphrase: &jump_passphrase,
        },
    )?;
    control.check()?;
    let saved = if use_saved && !save_key && password.is_empty() && endpoint.key.is_some() {
        crate::credentials::load(&endpoint)?
    } else {
        None
    };
    let key_data = imported
        .as_ref()
        .map(|key| key.as_str())
        .or_else(|| saved.as_ref().map(|s| s.private_key.as_str()));
    let phrase = if !passphrase.is_empty() {
        passphrase.as_str()
    } else {
        saved.as_ref().map(|s| s.passphrase.as_str()).unwrap_or("")
    };
    let used_key = authenticate(
        &session,
        &endpoint,
        &PrimaryAuth::new(&endpoint, &password, phrase, key_data),
        auth,
    )
    .map_err(|error| at_target(&endpoint, error))?;
    let sftp = session
        .sftp()
        .map_err(ssh_failure(SshStage::Channel))
        .map_err(|error| at_target(&endpoint, error))?;
    let requested = Location::Sftp {
        endpoint: endpoint.clone(),
        path: if path.is_empty() {
            ".".into()
        } else {
            path.into()
        },
    };
    let path = sftp
        .realpath(requested.path())
        .map_err(|source| AccessError::new(&requested, AccessAction::ResolvePath, source))?;
    let location = requested.with_path(path);
    let metadata = sftp
        .stat(location.path())
        .map_err(|source| AccessError::new(&location, AccessAction::ReadMetadata, source))?;
    match metadata.file_type() {
        FileType::Directory => {}
        FileType::Other(0) => return Err(ScanError::MissingSftpFileType(location).into()),
        _ => return Err(ConnectionError::InitialPathNotDirectory(location).into()),
    }
    control.finalize()?;
    let saved_profile = if let Some(mut key) = imported.filter(|_| used_key) {
        let secret = Zeroizing::new(crate::credentials::SavedKey {
            private_key: std::mem::take(&mut *key),
            passphrase: phrase.into(),
        });
        crate::credentials::save(&endpoint, &secret)?;
        true
    } else {
        false
    };
    let replaced = sessions()
        .lock()
        .map_err(err)?
        .insert(endpoint.clone(), Arc::new(session));
    drop(replaced);
    Ok(ConnectedLocation {
        location,
        saved_profile,
    })
}

pub(super) fn should_reopen(error: &ssh2::Error) -> bool {
    // libssh2's SFTP initializer can report CHANNEL_FAILURE for an
    // underlying transport error while it is opening the SSH channel.
    matches!(
        error.code(),
        ErrorCode::Session(
            LIBSSH2_ERROR_SOCKET_SEND
                | LIBSSH2_ERROR_SOCKET_RECV
                | LIBSSH2_ERROR_SOCKET_DISCONNECT
                | LIBSSH2_ERROR_SOCKET_TIMEOUT
                | LIBSSH2_ERROR_TIMEOUT
                | LIBSSH2_ERROR_CHANNEL_CLOSED
                | LIBSSH2_ERROR_CHANNEL_FAILURE
        ) | ErrorCode::SFTP(LIBSSH2_FX_NO_CONNECTION | LIBSSH2_FX_CONNECTION_LOST)
    )
}

pub(super) fn cached_channel(
    endpoint: &Endpoint,
    session: &Arc<Session>,
) -> Result<Option<Sftp>, TransferError> {
    match session.sftp() {
        Ok(channel) => Ok(Some(channel)),
        Err(error) if should_reopen(&error) => {
            let mut cache = sessions().lock().map_err(err)?;
            if cache
                .get(endpoint)
                .is_some_and(|current| Arc::ptr_eq(current, session))
            {
                cache.remove(endpoint);
            }
            Ok(None)
        }
        Err(error) => Err(ConnectionError::Automatic(ssh_failure(SshStage::Channel)(error)).into()),
    }
}

fn opening_gate(endpoint: &Endpoint) -> Result<Arc<OpeningGate>, String> {
    let mut opening = OPENING.get_or_init(Default::default).lock().map_err(err)?;
    opening.retain(|_, gate| gate.strong_count() != 0);
    let slot = opening.entry(endpoint.clone()).or_default();
    if let Some(gate) = slot.upgrade() {
        return Ok(gate);
    }
    let gate = Arc::new(Mutex::new(()));
    *slot = Arc::downgrade(&gate);
    Ok(gate)
}

pub(super) fn cached(endpoint: &Endpoint) -> Result<Option<Sftp>, TransferError> {
    let session = sessions().lock().map_err(err)?.get(endpoint).cloned();
    match session {
        Some(session) => cached_channel(endpoint, &session),
        None => Ok(None),
    }
}

pub(super) fn remote(location: &Location) -> Result<Sftp, TransferError> {
    let Location::Sftp { endpoint, .. } = location else {
        if location.is_archive() {
            return Err(MutationError::ReadOnlyArchive(location.clone()).into());
        }
        return Err(crate::i18n::text!("Ожидалось подключение SFTP").into());
    };
    if let Some(channel) = cached(endpoint)? {
        return Ok(channel);
    }
    let gate = opening_gate(endpoint)?;
    let _opening = gate.lock().map_err(err)?;
    if let Some(channel) = cached(endpoint)? {
        return Ok(channel);
    }
    // Reopen a channel before the caller issues a request. Never retry the
    // request itself or move an existing file handle to another SSH session.
    let session = open_connection(
        endpoint,
        &TrustFiles::user()?,
        None,
        #[cfg(target_os = "macos")]
        proxy_jump::Login {
            use_saved: true,
            passphrase: "",
        },
    )?;
    let saved = if endpoint.key.is_some() {
        crate::credentials::load(endpoint)?
    } else {
        None
    };
    login_primary(
        &session,
        &endpoint.user,
        PrimaryAuth::new(
            endpoint,
            "",
            saved.as_ref().map(|s| s.passphrase.as_str()).unwrap_or(""),
            saved.as_ref().map(|s| s.private_key.as_str()),
        ),
    )
    .map_err(ConnectionError::Automatic)
    .map_err(|error| at_target(endpoint, error.into()))?;
    let channel = session
        .sftp()
        .map_err(ssh_failure(SshStage::Channel))
        .map_err(ConnectionError::Automatic)
        .map_err(|error| at_target(endpoint, error.into()))?;
    // The SFTP dialog may have published a manually authenticated session
    // while this worker was opening its own channel.
    let session = Arc::new(session);
    sessions()
        .lock()
        .map_err(err)?
        .entry(endpoint.clone())
        .or_insert_with(|| Arc::clone(&session));
    Ok(channel)
}
