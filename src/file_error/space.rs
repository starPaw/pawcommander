use crate::vfs::Location;
use std::{error::Error, fmt, io, path::PathBuf};

#[derive(Debug)]
pub(crate) enum SpaceError {
    MissingDestinationParent(Location),
    LocalQuery {
        directory: PathBuf,
        source: io::Error,
    },
    Insufficient {
        directory: Location,
        required: u64,
        available: u128,
    },
    LocalWrite {
        directory: PathBuf,
        source: io::Error,
    },
    SftpProbe(Box<SftpSpaceFailure>),
}

#[derive(Debug)]
pub(crate) struct SftpSpaceFailure {
    pub(crate) directory: Location,
    pub(crate) probe: Location,
    pub(crate) causes: Vec<(ProbeAction, ssh2::Error)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeAction {
    Open,
    Query,
    Close,
    Remove,
}

impl fmt::Display for SpaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingDestinationParent(target) => {
                write!(f, "Destination has no parent folder: {target}")
            }
            Self::LocalQuery { directory, source } => {
                write!(
                    f,
                    "Could not check space in {}: {source}",
                    directory.display()
                )
            }
            Self::Insufficient {
                directory,
                required,
                available,
            } => {
                write!(f, "Not enough free space in {directory}: {required} bytes needed, {available} bytes available")
            }
            Self::LocalWrite { directory, source } => {
                write!(
                    f,
                    "Not enough free space while copying into {}: {source}",
                    directory.display()
                )
            }
            Self::SftpProbe(failure) => {
                write!(
                    f,
                    "Could not check SFTP space in {} with {}",
                    failure.directory, failure.probe
                )?;
                for (action, source) in &failure.causes {
                    write!(f, " · {action:?}: {source}")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for SpaceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LocalQuery { source, .. } | Self::LocalWrite { source, .. } => Some(source),
            Self::SftpProbe(failure) => failure.causes.first().map(|(_, source)| source as _),
            Self::MissingDestinationParent(_) | Self::Insufficient { .. } => None,
        }
    }
}
