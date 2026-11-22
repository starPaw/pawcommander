use crate::vfs::Endpoint;
use std::{error::Error, fmt};
use zeroize::Zeroize;

#[derive(Clone, Copy, Debug)]
pub(crate) enum CredentialAction {
    Read,
    Save,
    Forget,
}

#[derive(Debug)]
pub(crate) struct CredentialError {
    pub action: CredentialAction,
    pub profile: Box<Endpoint>,
    pub cause: CredentialFailure,
}

impl CredentialError {
    pub(crate) fn new(
        profile: &Endpoint,
        action: CredentialAction,
        cause: CredentialFailure,
    ) -> Self {
        Self {
            action,
            profile: Box::new(profile.clone()),
            cause,
        }
    }
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} saved SFTP credential for {}: {}",
            self.action,
            self.profile.label(),
            self.cause
        )
    }
}

impl Error for CredentialError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.cause)
    }
}

#[derive(Debug)]
pub(crate) enum CredentialFailure {
    Profile(serde_json::Error),
    Prepare(SecretFormatError),
    Malformed(Option<SecretFormatError>),
    #[cfg(target_os = "macos")]
    Keychain(security_framework::base::Error),
    StoreUnavailable,
    MissingEntry,
    AmbiguousEntry,
    InvalidEntry,
    AttributeTooLong {
        secret: bool,
        limit: u32,
    },
}

impl From<keyring::Error> for CredentialFailure {
    fn from(error: keyring::Error) -> Self {
        match error {
            #[cfg(target_os = "macos")]
            keyring::Error::PlatformFailure(source) | keyring::Error::NoStorageAccess(source) => {
                source
                    .downcast_ref::<security_framework::base::Error>()
                    .map_or(Self::StoreUnavailable, |status| Self::Keychain(*status))
            }
            keyring::Error::BadEncoding(mut bytes)
            | keyring::Error::BadDataFormat(mut bytes, _) => {
                bytes.zeroize();
                Self::Malformed(None)
            }
            keyring::Error::NoEntry => Self::MissingEntry,
            keyring::Error::Ambiguous(_) => Self::AmbiguousEntry,
            keyring::Error::Invalid(..) => Self::InvalidEntry,
            keyring::Error::TooLong(field, limit) => Self::AttributeTooLong {
                secret: field == "secret",
                limit,
            },
            _ => Self::StoreUnavailable,
        }
    }
}

impl fmt::Display for CredentialFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Profile(source) => write!(f, "Could not identify the SFTP profile: {source}"),
            Self::Prepare(source) => write!(f, "Could not serialize the saved key: {source}"),
            Self::Malformed(Some(source)) => write!(f, "Malformed saved key: {source}"),
            Self::Malformed(None) => f.write_str("Malformed saved key"),
            #[cfg(target_os = "macos")]
            Self::Keychain(source) => write!(f, "Keychain error {}: {source}", source.code()),
            Self::StoreUnavailable => f.write_str("Credential store unavailable"),
            Self::MissingEntry => f.write_str("Credential entry missing"),
            Self::AmbiguousEntry => f.write_str("Multiple credential entries match"),
            Self::InvalidEntry => f.write_str("Invalid credential entry"),
            Self::AttributeTooLong { secret, limit } => {
                let field = if *secret { "secret" } else { "attribute" };
                write!(f, "Credential {field} exceeds the {limit}-byte limit")
            }
        }
    }
}

impl Error for CredentialFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Profile(source) => Some(source),
            Self::Prepare(source) | Self::Malformed(Some(source)) => Some(source),
            #[cfg(target_os = "macos")]
            Self::Keychain(source) => Some(source),
            _ => None,
        }
    }
}

// Serde's error message can quote a value from the stored key or passphrase.
#[derive(Debug)]
pub(crate) struct SecretFormatError {
    category: serde_json::error::Category,
    line: usize,
    column: usize,
}

impl From<serde_json::Error> for SecretFormatError {
    fn from(error: serde_json::Error) -> Self {
        Self {
            category: error.classify(),
            line: error.line(),
            column: error.column(),
        }
    }
}

impl fmt::Display for SecretFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "JSON {:?} at line {}, column {}",
            self.category, self.line, self.column
        )
    }
}

impl Error for SecretFormatError {}
