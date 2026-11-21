use crate::archive;
use crate::file_error::PathError;
use crate::operations::TransferError;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) jump: Option<Box<JumpHost>>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct JumpHost {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key: Option<PathBuf>,
}

impl JumpHost {
    #[cfg(target_os = "macos")]
    pub(crate) fn label(&self) -> String {
        format!("{}@{} · {}", self.user, self.host, self.port)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn endpoint(&self) -> Endpoint {
        Endpoint {
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone(),
            key: self.key.clone(),
            jump: None,
        }
    }
}

impl Endpoint {
    pub(crate) fn label(&self) -> String {
        format!("{}@{} · {}", self.user, self.host, self.port)
    }

    pub(crate) fn host_name(&self) -> &str {
        self.host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .filter(|host| host.parse::<std::net::Ipv6Addr>().is_ok())
            .unwrap_or(&self.host)
    }

    fn same_host_port(&self, other: &Self) -> bool {
        let left = self.host_name();
        let right = other.host_name();
        let same_host = left.eq_ignore_ascii_case(right)
            || matches!(
                (left.parse::<std::net::Ipv6Addr>(), right.parse::<std::net::Ipv6Addr>()),
                (Ok(left), Ok(right)) if left == right
            );
        same_host && self.port == other.port
    }

    pub(crate) fn same_authority(&self, other: &Self) -> bool {
        self.same_host_port(other) && self.user == other.user
    }

    pub(super) fn same_route(&self, other: &Self) -> bool {
        self.same_authority(other) && self.jump == other.jump
    }
}

// Untagged keeps persisted local paths compatible with the original settings.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Location {
    Local(PathBuf),
    Sftp {
        endpoint: Endpoint,
        path: PathBuf,
    },
    Archive {
        archive: Arc<PathBuf>,
        path: PathBuf,
    },
}

impl From<PathBuf> for Location {
    fn from(path: PathBuf) -> Self {
        Self::Local(path)
    }
}

impl Location {
    pub fn path(&self) -> &Path {
        match self {
            Self::Local(p) | Self::Sftp { path: p, .. } | Self::Archive { path: p, .. } => p,
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
    pub(crate) fn sftp_endpoint(&self) -> Option<&Endpoint> {
        match self {
            Self::Sftp { endpoint, .. } => Some(endpoint),
            _ => None,
        }
    }
    pub(crate) fn archive_source(&self) -> Option<&Path> {
        match self {
            Self::Archive { archive, .. } => Some(archive),
            _ => None,
        }
    }
    pub(crate) fn is_archive(&self) -> bool {
        self.archive_source().is_some()
    }
    pub(crate) fn browse_archive(&self) -> Option<Self> {
        let path = self.local()?;
        archive::Format::from_path(path)?;
        Some(Self::Archive {
            archive: Arc::new(path.to_owned()),
            path: PathBuf::new(),
        })
    }
    pub(crate) fn has_utf8_paths(&self) -> bool {
        self.path().to_str().is_some()
            && match self {
                Self::Local(_) => true,
                Self::Archive { archive, .. } => archive.to_str().is_some(),
                Self::Sftp { endpoint, .. } => {
                    let jump_key = endpoint.jump.as_ref().and_then(|jump| jump.key.as_deref());
                    endpoint
                        .key
                        .iter()
                        .map(PathBuf::as_path)
                        .chain(jump_key)
                        .all(|key| key.to_str().is_some())
                }
            }
    }
    pub fn with_path(&self, path: PathBuf) -> Self {
        match self {
            Self::Local(_) => Self::Local(path),
            Self::Archive { archive, .. } => Self::Archive {
                archive: Arc::clone(archive),
                path,
            },
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
        if let Self::Archive { archive, path } = self
            && path.as_os_str().is_empty()
        {
            return archive
                .parent()
                .map(|parent| Self::Local(parent.to_owned()));
        }
        self.path().parent().map(|p| self.with_path(p.into()))
    }
    pub fn ancestors(&self) -> impl Iterator<Item = Self> + '_ {
        std::iter::successors(Some(self.clone()), Self::parent)
    }
    pub fn display(&self) -> String {
        self.to_string()
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn with_sftp_route_label(&self, label: String) -> String {
        match self
            .sftp_endpoint()
            .and_then(|endpoint| endpoint.jump.as_deref())
        {
            Some(jump) => format!("{label} · ProxyJump {}", jump.label()),
            None => label,
        }
    }
    pub fn name(&self) -> String {
        let name = self
            .path()
            .file_name()
            .or_else(|| self.archive_source()?.file_name());
        name.map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.to_string())
    }
    pub(crate) fn same_backend(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local(_), Self::Local(_)) => true,
            (Self::Sftp { endpoint: a, .. }, Self::Sftp { endpoint: b, .. }) => a == b,
            (Self::Archive { archive: a, .. }, Self::Archive { archive: b, .. }) => a == b,
            _ => false,
        }
    }
    pub(crate) fn same_sftp_authority(&self, other: &Self) -> bool {
        self.sftp_endpoint()
            .zip(other.sftp_endpoint())
            .is_some_and(|(a, b)| a.same_route(b))
    }
    pub(super) fn might_share_filesystem(&self, other: &Self) -> bool {
        self.same_backend(other)
            || self
                .sftp_endpoint()
                .zip(other.sftp_endpoint())
                .is_some_and(|(a, b)| a.same_host_port(b) && (a.jump.is_some() || b.jump.is_some()))
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(p) => write!(f, "{}", p.display()),
            Self::Archive { archive, path } => {
                let archive = percent_encoding::percent_encode(
                    archive.as_os_str().as_encoded_bytes(),
                    ARCHIVE_ADDRESS_ESCAPE,
                );
                let member = if cfg!(windows) {
                    path.to_string_lossy()
                        .replace('\\', "/")
                        .into_bytes()
                        .into()
                } else {
                    std::borrow::Cow::Borrowed(path.as_os_str().as_encoded_bytes())
                };
                let member = percent_encoding::percent_encode(&member, ARCHIVE_ADDRESS_ESCAPE);
                write!(f, "archive:{archive}!/{member}")
            }
            Self::Sftp { endpoint, path } => {
                let mut url = url::Url::parse("sftp://localhost/").map_err(|_| fmt::Error)?;
                let host = endpoint.host_name();
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
                // The URL path setter discards literal tabs and line breaks.
                // Escape controls and filename percent signs before passing it the path.
                let escaped: std::borrow::Cow<'_, str> =
                    percent_encoding::utf8_percent_encode(&path, SFTP_ADDRESS_PATH_ESCAPE).into();
                url.set_path(&escaped);
                write!(f, "{url}")
            }
        }
    }
}

const SFTP_ADDRESS_PATH_ESCAPE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b'%');
pub(super) const ARCHIVE_ADDRESS_ESCAPE: &percent_encoding::AsciiSet =
    &percent_encoding::CONTROLS.add(b'%').add(b'!');

pub fn resolve(address: &str, current: &Location) -> Result<Location, TransferError> {
    if let Some(address) = address.strip_prefix("archive:") {
        let (file, member) = address.split_once("!/").ok_or(PathError::ArchiveAddress)?;
        let file = percent_decode_str(file).collect::<Vec<_>>();
        if file.contains(&0) {
            return Err(PathError::ArchiveAddress.into());
        }
        #[cfg(unix)]
        let file = {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(OsString::from_vec(file))
        };
        #[cfg(not(unix))]
        let file = PathBuf::from(String::from_utf8(file).map_err(|_| PathError::ArchiveAddress)?);
        if !file.is_absolute() || archive::Format::from_path(&file).is_none() {
            return Err(PathError::ArchiveFile(file).into());
        }
        let path = archive::parse_member_path(&percent_decode_str(member).collect::<Vec<_>>())
            .map_err(|source| PathError::ArchiveMember {
                archive: file.clone(),
                source,
            })?;
        return Ok(Location::Archive {
            archive: Arc::new(file),
            path,
        });
    }
    if address.starts_with("sftp:") {
        let url = url::Url::parse(address).map_err(PathError::SftpUrl)?;
        if url.scheme() != "sftp"
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(PathError::SftpUrlFields.into());
        }
        let host = url.host_str().ok_or(PathError::MissingSftpHost)?.to_owned();
        let user = percent_decode_str(url.username())
            .decode_utf8()
            .map_err(PathError::SftpUserEncoding)?
            .into_owned();
        if user.is_empty() {
            return Err(PathError::MissingSftpUser.into());
        }
        let mut endpoint = Endpoint {
            host,
            port: url.port().unwrap_or(22),
            user,
            key: None,
            jump: None,
        };
        if let Some(existing) = current
            .sftp_endpoint()
            .filter(|existing| endpoint.same_host_port(existing))
        {
            if endpoint.user == existing.user {
                // Keep the exact session key. URL rendering may normalize the hostname.
                endpoint = existing.clone();
            } else {
                // Changing the account keeps the route to this server and uses new credentials.
                endpoint.jump.clone_from(&existing.jump);
            }
        }
        let path = percent_decode_str(url.path())
            .decode_utf8()
            .map_err(PathError::SftpPathEncoding)?;
        if path.contains('\0') {
            return Err(PathError::SftpPathContainsNul(PathBuf::from(path.as_ref())).into());
        }
        return Ok(Location::Sftp {
            endpoint,
            path: normalize_remote(Path::new(if path.is_empty() { "/" } else { &path }))?,
        });
    }
    if current.remote() {
        if address == "~" || address.starts_with("~/") {
            return Err(PathError::SftpHomeShorthand.into());
        }
        let path = if address.starts_with('/') {
            PathBuf::from(address)
        } else {
            current.join(address).path().to_owned()
        };
        return Ok(current.with_path(normalize_remote(&path)?));
    }
    if let Some(parent) = current.archive_source().and_then(Path::parent) {
        return resolve(address, &Location::Local(parent.to_owned()));
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

pub(super) fn normalize_remote(path: &Path) -> Result<PathBuf, TransferError> {
    let text = path
        .to_str()
        .ok_or_else(|| PathError::RemotePathEncoding(path.to_owned()))?;
    if !text.starts_with('/') || text.contains(['\0', '\\']) {
        return Err(PathError::RemotePathSyntax(path.to_owned()).into());
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

pub(super) fn validate_name(name: &str) -> Result<(), PathError> {
    let mut components = Path::new(name).components();
    if name.trim().is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', '\0'])
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(PathError::InvalidName(name.to_owned()));
    }
    Ok(())
}
