use super::{
    discovery, mount_id, root, PathAction, SmbError, CONNECTION_PREFIX, MOUNT_DIR, RECORD,
};
use crate::file_error::{ListingError, ListingErrorKind};
use crate::operations::TransferError;
use crate::vfs::Location;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::{fs, io};

/// Saved layouts and navigation history can refer to SMB folders using local paths.
pub(crate) fn check_location(location: &Location) -> Result<(), TransferError> {
    if let Some(path) = location.local().or_else(|| location.archive_source()) {
        check_path(path, root, |mount| mount_id(mount, true)).map_err(|cause| {
            ListingError::new(
                location.clone(),
                ListingErrorKind::SmbMount(Box::new(cause)),
            )
        })?;
    }
    Ok(())
}

fn check_path(
    path: &Path,
    storage: impl FnOnce() -> Result<PathBuf, SmbError>,
    inspect: impl FnOnce(&Path) -> io::Result<Option<[i32; 2]>>,
) -> Result<(), SmbError> {
    let mut candidates = path
        .ancestors()
        .filter_map(|mount| {
            mount.file_name()?;
            let parent = mount.parent()?;
            let base = parent.parent()?;
            let name = base.file_name()?;
            (parent.file_name()? == MOUNT_DIR
                && name.as_bytes().starts_with(CONNECTION_PREFIX.as_bytes()))
            .then_some((base, mount, name))
        })
        .peekable();
    // Ordinary paths need no storage access. Compare registry aliases without
    // resolving the share path, which could touch a different volume.
    if candidates.peek().is_none() {
        return Ok(());
    }
    let Ok(root) = storage() else {
        return Ok(());
    };
    let canonical = root.canonicalize().ok();
    let Some((_, mount, name)) = candidates.find(|(base, _, _)| {
        base.parent()
            .is_some_and(|parent| parent == root.as_path() || Some(parent) == canonical.as_deref())
    }) else {
        return Ok(());
    };
    let base = canonical.as_deref().unwrap_or(&root).join(name);
    let record_path = base.join(RECORD);
    match fs::symlink_metadata(&base) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(SmbError::InvalidConnectionDirectory(base)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(SmbError::MissingRecord(record_path));
        }
        Err(error) => return Err(PathAction::ReadStorage.at(&base)(error)),
    }
    let share = discovery::read_saved_share(&base)?.ok_or(SmbError::MissingRecord(record_path))?;
    if share.path.file_name() != mount.file_name()
        || !path.strip_prefix(mount).is_ok_and(|suffix| {
            suffix
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        })
    {
        return Err(SmbError::PathOutsideShare {
            path: path.to_owned(),
            mount: share.path,
        });
    }
    share
        .require_expected_mount(inspect)
        .map_err(|cause| share.open_error(cause))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smb::{save_record, Endpoint, Record};

    #[test]
    fn saved_paths_check_the_owned_share_and_its_identity_through_registry_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("registry");
        let base = root.join("connection-fixture");
        let record = Record {
            endpoint: Endpoint::parse("smb://nas/Files", "", true).unwrap(),
            id: [1, 2],
        };
        fs::create_dir_all(&base).unwrap();
        save_record(&base, &record).unwrap();
        let mount = record.endpoint.mountpoint(&base);
        let alias = root.with_file_name("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        for requested_root in [&root, &alias] {
            let requested = requested_root
                .join(mount.strip_prefix(&root).unwrap())
                .join("nested");
            check_path(
                &requested,
                || Ok(alias.clone()),
                |path| {
                    assert_eq!(path, mount);
                    Ok(Some(record.id))
                },
            )
            .unwrap();
            let error =
                check_path(&requested, || Ok(alias.clone()), |_| Ok(Some([3, 4]))).unwrap_err();
            assert!(matches!(error, SmbError::OpenShare { cause, .. }
                if matches!(cause.as_ref(), SmbError::MountChanged(path) if path == &mount)));
        }
        let nested = mount.join("connection-nested/mount/Ordinary Folder");
        check_path(
            &nested,
            || Ok(root.clone()),
            |path| {
                assert_eq!(path, mount);
                Ok(Some(record.id))
            },
        )
        .unwrap();
        for invalid in [mount.with_file_name("Other"), mount.join("../Other")] {
            assert!(
                matches!(check_path(&invalid, || Ok(root.clone()), |_| panic!("invalid path")),
                Err(SmbError::PathOutsideShare { path, mount: expected }) if path == invalid && expected == mount)
            );
        }
    }

    #[test]
    fn missing_managed_records_and_unrelated_local_paths_are_distinguished() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("registry");
        let ordinary = root
            .with_file_name("ordinary")
            .join("connection-fixture/mount/Files");
        check_path(
            &ordinary,
            || Ok(root.clone()),
            |_| panic!("outside registry"),
        )
        .unwrap();
        check_path(
            &root.with_file_name("ordinary").join("normal/folder"),
            || panic!("no registry lookup"),
            |_| panic!("ordinary path"),
        )
        .unwrap();

        let base = root.join("connection-fixture");
        let path = base.join("mount/Files/child");
        assert!(
            matches!(check_path(&path, || Ok(root.clone()), |_| panic!("no record")),
            Err(SmbError::MissingRecord(record)) if record == base.join(RECORD))
        );
        fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(temp.path(), &base).unwrap();
        assert!(
            matches!(check_path(&path, || Ok(root.clone()), |_| panic!("redirected directory")),
            Err(SmbError::InvalidConnectionDirectory(directory)) if directory == base)
        );
    }
}
