use std::{error::Error, fmt, io, path::PathBuf, str::Utf8Error};

#[derive(Debug)]
pub(crate) enum HostKeyError {
    HomeUnavailable,
    KeyUnavailable,
    Unverified,
    PinDirectoryUnavailable,
    KnownHosts {
        path: PathBuf,
        cause: KnownHostsError,
    },
    KnownHostsChanged {
        path: PathBuf,
        received: String,
    },
    Pin {
        path: PathBuf,
        cause: PinError,
    },
}

impl fmt::Display for HostKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeUnavailable => f.write_str("Home directory could not be located"),
            Self::KeyUnavailable => f.write_str("The server did not provide an SSH host key"),
            Self::Unverified => f.write_str("The SSH host key is unverified"),
            Self::PinDirectoryUnavailable => f.write_str("The SSH pin directory is unavailable"),
            Self::KnownHosts { path, cause } => {
                write!(f, "{}: {cause}", path.display())
            }
            Self::KnownHostsChanged { path, received } => {
                write!(
                    f,
                    "The SSH host key changed in {} (received {received})",
                    path.display()
                )
            }
            Self::Pin { path, cause } => write!(f, "{}: {cause}", path.display()),
        }
    }
}

impl Error for HostKeyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::KnownHosts { cause, .. } => Some(cause),
            Self::Pin { cause, .. } => Some(cause),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum KnownHostsError {
    Read(io::Error),
    Initialize(ssh2::Error),
    Entry {
        line: usize,
        cause: KnownHostEntryError,
    },
    Check,
    Revoked,
    CertificateAuthority,
}

impl fmt::Display for KnownHostsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(source) => fmt::Display::fmt(source, f),
            Self::Initialize(source) => fmt::Display::fmt(source, f),
            Self::Entry { line, cause } => write!(f, "known_hosts line {line}: {cause}"),
            Self::Check => f.write_str("SSH known_hosts check failed"),
            Self::Revoked => f.write_str("The SSH host key is revoked"),
            Self::CertificateAuthority => f.write_str("The SSH host uses a certificate authority"),
        }
    }
}

impl Error for KnownHostsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(source) => Some(source),
            Self::Initialize(source) => Some(source),
            Self::Entry { cause, .. } => Some(cause),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum KnownHostEntryError {
    Format,
    Encoding(Utf8Error),
    Parse(ssh2::Error),
}

impl fmt::Display for KnownHostEntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format => f.write_str("Invalid known_hosts entry"),
            Self::Encoding(source) => fmt::Display::fmt(source, f),
            Self::Parse(source) => fmt::Display::fmt(source, f),
        }
    }
}

impl Error for KnownHostEntryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encoding(source) => Some(source),
            Self::Parse(source) => Some(source),
            Self::Format => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum PinError {
    Read(io::Error),
    Write(io::Error),
    Json(serde_json::Error),
    TooLarge,
    Changed { saved: String, received: String },
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(source) | Self::Write(source) => fmt::Display::fmt(source, f),
            Self::Json(source) => fmt::Display::fmt(source, f),
            Self::TooLarge => f.write_str("The trusted SSH key record is too large"),
            Self::Changed { saved, received } => {
                write!(
                    f,
                    "The SSH host key changed (saved {saved}, received {received})"
                )
            }
        }
    }
}

impl Error for PinError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(source) | Self::Write(source) => Some(source),
            Self::Json(source) => Some(source),
            _ => None,
        }
    }
}
