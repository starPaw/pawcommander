use super::{
    Endpoint, MountedShare, PathAction, Record, RecordProblem, SmbError, CONNECTION_PREFIX,
    MOUNT_DIR, RECORD,
};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::Path,
};
use zeroize::Zeroizing;

const MAX_RECORD_BYTES: usize = 16 * 1024;
const MAX_WARNINGS: usize = 16;

#[derive(Default)]
pub(crate) struct Discovery {
    pub(crate) shares: Vec<MountedShare>,
    pub(crate) warnings: DiscoveryWarnings,
}

#[derive(Default)]
pub(crate) struct DiscoveryWarnings {
    pub(crate) entries: Vec<SmbError>,
    pub(crate) omitted: usize,
}

impl DiscoveryWarnings {
    pub(crate) fn push(&mut self, error: SmbError) {
        if self.entries.len() < MAX_WARNINGS {
            self.entries.push(error);
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }
}

pub(super) fn scan(
    root: &Path,
    inspect: impl Fn(&Path) -> io::Result<Option<[i32; 2]>>,
) -> Discovery {
    let mut discovered = Discovery::default();
    let entries = root
        .canonicalize()
        .map_err(PathAction::ResolveStorage.at(root))
        .and_then(|path| fs::read_dir(&path).map_err(PathAction::ReadStorage.at(&path)));
    let entries = match entries {
        Ok(entries) => entries,
        Err(SmbError::Local { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return discovered;
        }
        Err(error) => {
            discovered.warnings.push(error);
            return discovered;
        }
    };
    for entry in entries {
        let result = entry
            .map_err(PathAction::ReadStorage.at(root))
            .and_then(|entry| {
                if !entry
                    .file_name()
                    .as_bytes()
                    .starts_with(CONNECTION_PREFIX.as_bytes())
                {
                    return Ok(None);
                }
                let path = entry.path();
                if !entry
                    .file_type()
                    .map_err(PathAction::ReadStorage.at(&path))?
                    .is_dir()
                {
                    return Ok(None);
                }
                read_share(&path, &inspect)
            });
        match result {
            Ok(Some(share)) => discovered.shares.push(share),
            Ok(None) => {}
            Err(error) => discovered.warnings.push(error),
        }
    }
    discovered
}

fn read_share(
    base: &Path,
    inspect: &impl Fn(&Path) -> io::Result<Option<[i32; 2]>>,
) -> Result<Option<MountedShare>, SmbError> {
    let Some(share) = read_saved_share(base)? else {
        check_unrecorded_mount(base, &base.join(RECORD), inspect)?;
        return Ok(None);
    };
    Ok(share.has_expected_mount(inspect)?.then_some(share))
}

pub(super) fn read_saved_share(base: &Path) -> Result<Option<MountedShare>, SmbError> {
    let record_path = base.join(RECORD);
    let Some(record) = read_record(&record_path)? else {
        return Ok(None);
    };
    let invalid = |problem| SmbError::InvalidRecord {
        path: record_path.clone(),
        problem,
    };
    let endpoint = Endpoint::parse(
        &record.endpoint.address,
        record.endpoint.user.as_deref().unwrap_or(""),
        record.endpoint.user.is_none(),
    )
    .map_err(|error| invalid(RecordProblem::Endpoint(Box::new(error))))?;
    if endpoint != record.endpoint {
        return Err(invalid(RecordProblem::EndpointMismatch));
    }
    Ok(Some(MountedShare {
        path: endpoint.mountpoint(base),
        base: base.to_owned(),
        record,
    }))
}

fn read_record(path: &Path) -> Result<Option<Record>, SmbError> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(PathAction::ReadRecord.at(path)(error)),
    };
    let invalid = |problem| SmbError::InvalidRecord {
        path: path.to_owned(),
        problem,
    };
    if !file
        .metadata()
        .map_err(PathAction::ReadRecord.at(path))?
        .is_file()
    {
        return Err(invalid(RecordProblem::NotRegular));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take((MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(PathAction::ReadRecord.at(path))?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(invalid(RecordProblem::TooLarge {
            limit: MAX_RECORD_BYTES,
        }));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| invalid(RecordProblem::Json(error.into())))
}

fn check_unrecorded_mount(
    base: &Path,
    record: &Path,
    inspect: &impl Fn(&Path) -> io::Result<Option<[i32; 2]>>,
) -> Result<(), SmbError> {
    let parent = base.join(MOUNT_DIR);
    let metadata = match fs::symlink_metadata(&parent) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(PathAction::ReadStorage.at(&parent)(error)),
    };
    if !metadata.is_dir() {
        return Ok(());
    }
    // Read the local parent and inspect the mount table without opening the share.
    for entry in fs::read_dir(&parent).map_err(PathAction::ReadStorage.at(&parent))? {
        let mount = entry.map_err(PathAction::ReadStorage.at(&parent))?.path();
        if inspect(&mount)
            .map_err(PathAction::InspectMount.at(&mount))?
            .is_some()
        {
            return Err(SmbError::UnrecordedMount {
                mount,
                record: record.to_owned(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn save(root: &Path, name: &str, record: &Record) -> PathBuf {
        let base = root.join(name);
        fs::create_dir(&base).unwrap();
        fs::write(base.join(RECORD), serde_json::to_vec(record).unwrap()).unwrap();
        record.endpoint.mountpoint(&base)
    }

    #[test]
    fn damaged_records_leave_valid_mounts_visible_and_preserve_original_causes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let record = Record {
            endpoint: Endpoint::parse("smb://server/share", "", true).unwrap(),
            id: [1, 2],
        };
        let healthy = save(&root, "connection-healthy", &record);
        let changed = save(&root, "connection-changed", &record);
        let denied = save(&root, "connection-denied", &record);
        let _detached = save(&root, "connection-detached", &record);

        let malformed = root.join("connection-malformed").join(RECORD);
        fs::create_dir(malformed.parent().unwrap()).unwrap();
        fs::write(&malformed, br#"{"id":"private-value-in-malformed-record"}"#).unwrap();
        let oversized = root.join("connection-oversized").join(RECORD);
        fs::create_dir(oversized.parent().unwrap()).unwrap();
        fs::write(&oversized, vec![b' '; MAX_RECORD_BYTES + 1]).unwrap();
        let linked = root.join("connection-linked").join(RECORD);
        fs::create_dir(linked.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&malformed, &linked).unwrap();

        let discovered = scan(&root, |path| {
            if path == healthy {
                Ok(Some(record.id))
            } else if path == changed {
                Ok(Some([9, 9]))
            } else if path == denied {
                Err(io::Error::from_raw_os_error(libc::EACCES))
            } else {
                Ok(None)
            }
        });
        let [mounted] = discovered.shares.as_slice() else {
            panic!("expected one verified share");
        };
        assert_eq!(mounted.path, healthy);
        let warnings = &discovered.warnings.entries;
        assert_eq!(warnings.len(), 5, "{warnings:?}");
        assert!(warnings
            .iter()
            .any(|error| matches!(error, SmbError::MountChanged(path) if path == &changed)));
        assert!(warnings.iter().any(|error| matches!(error,
            SmbError::Local { action: PathAction::InspectMount, path, source }
                if path == &denied && source.raw_os_error() == Some(libc::EACCES))));
        let json = warnings
            .iter()
            .find(|error| {
                matches!(error,
            SmbError::InvalidRecord { path, problem: RecordProblem::Json(_) } if path == &malformed)
            })
            .unwrap();
        let rendered = crate::i18n::smb_error(json);
        assert!(rendered.contains(malformed.to_str().unwrap()));
        assert!(
            !format!("{json:?} {json} {rendered}").contains("private-value-in-malformed-record")
        );
        assert!(warnings.iter().any(|error| matches!(error,
            SmbError::InvalidRecord { path, problem: RecordProblem::TooLarge { limit: MAX_RECORD_BYTES } }
                if path == &oversized)));
        assert!(warnings.iter().any(|error| matches!(error,
            SmbError::Local { action: PathAction::ReadRecord, path, source }
                if path == &linked && source.raw_os_error() == Some(libc::ELOOP))));
        assert!(linked.is_symlink() && malformed.is_file() && oversized.is_file());
    }

    #[test]
    fn missing_record_is_reported_only_for_an_observed_mount_and_warnings_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        assert!(scan(&root.join("uninitialized"), |_| panic!("no storage"))
            .warnings
            .entries
            .is_empty());
        let mount = root.join("connection-incomplete/mount/share");
        fs::create_dir_all(&mount).unwrap();
        assert!(scan(&root, |_| Ok(None)).warnings.entries.is_empty());
        let mut result = scan(&root, |path| Ok((path == mount).then_some([1, 2])));
        assert!(result.shares.is_empty());
        let [SmbError::UnrecordedMount {
            mount: found,
            record,
        }] = result.warnings.entries.as_slice()
        else {
            panic!("expected one unrecorded mount warning");
        };
        assert_eq!(found, &mount);
        assert_eq!(record, &root.join("connection-incomplete").join(RECORD));
        for _ in 0..MAX_WARNINGS {
            result.warnings.push(SmbError::WorkerDisconnected);
        }
        assert_eq!(result.warnings.entries.len(), MAX_WARNINGS);
        assert_eq!(result.warnings.omitted, 1);
        assert!(mount.is_dir());
    }
}
