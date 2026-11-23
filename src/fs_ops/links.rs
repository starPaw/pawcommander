use super::{directory_path, validate_name};
use crate::file_error::{
    AccessAction, AccessError, LinkCreateError, LinkIoAction, SmbMutationAction,
};
use crate::operations::{Progress, TransferError};
use crate::smb::MutationMount;
use crate::vfs::{with_open_file_staging, Location, FILE_STAGING_PAYLOAD};
use rustix::fs::{
    fstat, linkat, lstat, renameat_with, statat, symlinkat, AtFlags, FileType, RenameFlags, Stat,
};
use std::fs::{self, Metadata, OpenOptions};
use std::io;
use std::os::{
    fd::OwnedFd,
    unix::fs::{symlink, OpenOptionsExt},
};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Kind {
    #[default]
    Symbolic,
    Hard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkStatus {
    Available,
    Missing,
    Loop,
    Unavailable,
}

impl LinkStatus {
    pub(super) fn from_stat(stat: &io::Result<Metadata>) -> Self {
        match stat {
            Ok(_) => Self::Available,
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => Self::Loop,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                Self::Missing
            }
            Err(_) => Self::Unavailable,
        }
    }
}

#[derive(Debug)]
pub(crate) struct LinkTarget {
    pub(crate) path: PathBuf,
    pub(crate) status: LinkStatus,
}

pub(crate) fn read_target(path: &Path) -> Result<LinkTarget, Box<AccessError>> {
    let target = fs::read_link(path).map_err(|error| {
        Box::new(AccessError::new(
            &Location::Local(path.to_owned()),
            AccessAction::ReadLink,
            error,
        ))
    })?;
    // The kernel resolves relative targets at the link and bounds chain traversal.
    let status = LinkStatus::from_stat(&fs::metadata(path));
    Ok(LinkTarget {
        path: target,
        status,
    })
}

pub(crate) struct CreatedLink {
    pub(crate) destination: PathBuf,
    pub(crate) cleanup_warning: Option<TransferError>,
}

pub(crate) fn create(
    directory: &Path,
    name: &str,
    target: &Path,
    kind: Kind,
    progress: &mut Progress,
) -> Result<CreatedLink, TransferError> {
    validate_name(name).map_err(LinkCreateError::InvalidName)?;
    if target.as_os_str().is_empty() {
        return Err(LinkCreateError::MissingTarget.into());
    }
    let mut mount = MutationMount::new(SmbMutationAction::CreateLink);
    mount.check(&directory.join(name))?;
    let directory = directory_path(directory)?;
    let destination = directory.join(name);
    mount.check(&destination)?;
    progress.checkpoint()?;
    let cleanup_warning = match kind {
        Kind::Symbolic => {
            let result = if let Some(parent) = mount.open_parent(&destination)? {
                symlinkat(target, &parent, name).map_err(io::Error::from)
            } else {
                symlink(target, &destination)
            };
            result
                .map_err(|error| LinkCreateError::io(&destination, LinkIoAction::Create, error))?;
            None
        }
        Kind::Hard => create_hard_link(&directory, target, &destination, &mount, progress)?,
    };
    Ok(CreatedLink {
        destination,
        cleanup_warning,
    })
}

fn create_hard_link(
    directory: &Path,
    target: &Path,
    destination: &Path,
    mount: &MutationMount,
    progress: &mut Progress,
) -> Result<Option<TransferError>, TransferError> {
    let inspect_directory =
        |error| LinkCreateError::io(directory, LinkIoAction::InspectDirectory, error);
    let parent = match mount.open_parent(destination)? {
        Some(parent) => parent,
        None => OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_SEARCH | libc::O_NOFOLLOW)
            .open(directory)
            .map_err(inspect_directory)?,
    };
    let parent = OwnedFd::from(parent);
    let expected_parent = fstat(&parent).map_err(|error| inspect_directory(error.into()))?;

    let source = directory.join(target);
    let mut source_mount = MutationMount::new(SmbMutationAction::InspectLinkTarget);
    // A hard-link target may use "..". Check the named share before resolving
    // those components, then check the actual parent without following the file.
    let initial_path: PathBuf = source
        .components()
        .take_while(|component| *component != Component::ParentDir)
        .collect();
    source_mount.check(&initial_path)?;
    let Some((source_parent, source_name)) = source.parent().zip(source.file_name()) else {
        return Err(LinkCreateError::ExpectedFile(source).into());
    };
    let inspect_target = |error| LinkCreateError::io(&source, LinkIoAction::InspectTarget, error);
    let resolved_source = fs::canonicalize(source_parent)
        .map_err(inspect_target)?
        .join(source_name);
    if resolved_source != initial_path {
        source_mount.check(&resolved_source)?;
    }
    let expected = lstat(&source).map_err(|error| inspect_target(error.into()))?;
    if FileType::from_raw_mode(expected.st_mode) != FileType::RegularFile {
        return Err(LinkCreateError::ExpectedFile(source).into());
    }
    if expected.st_dev != expected_parent.st_dev {
        return Err(LinkCreateError::DifferentVolume(source).into());
    }
    let destination_name = destination.file_name().expect("validated link name");
    let create_error = |error| LinkCreateError::io(destination, LinkIoAction::Create, error);
    match statat(&parent, destination_name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => return Err(create_error(io::Error::from_raw_os_error(libc::EEXIST)).into()),
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(create_error(error.into()).into()),
    }

    progress.checkpoint()?;
    mount.recheck()?;
    source_mount.recheck()?;
    let mut published = false;
    let result = with_open_file_staging(destination, None, &parent, |stage, handle, _| {
        mount.verify_directory(handle, stage.path(), destination)?;
        linkat(
            &parent,
            target,
            handle,
            FILE_STAGING_PAYLOAD,
            AtFlags::empty(),
        )
        .map_err(|error| create_error(error.into()))?;
        // Validate the link the OS actually made before it can become visible.
        let made = statat(handle, FILE_STAGING_PAYLOAD, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| create_error(error.into()))?;
        if FileType::from_raw_mode(made.st_mode) != FileType::RegularFile
            || !same_file(&made, &expected)
        {
            return Err(LinkCreateError::TargetChanged(source).into());
        }

        progress.checkpoint()?;
        mount.recheck()?;
        source_mount.recheck()?;
        let latest_parent = lstat(directory).map_err(|error| inspect_directory(error.into()))?;
        if FileType::from_raw_mode(latest_parent.st_mode) != FileType::Directory
            || !same_file(&latest_parent, &expected_parent)
        {
            return Err(LinkCreateError::DirectoryChanged(directory.to_owned()).into());
        }
        renameat_with(
            handle,
            FILE_STAGING_PAYLOAD,
            &parent,
            destination_name,
            RenameFlags::NOREPLACE,
        )
        .map_err(|error| create_error(error.into()))?;
        published = true;
        Ok(())
    });
    match result {
        Ok(()) => Ok(None),
        Err(error) if published => Ok(Some(error)),
        Err(error) => Err(error),
    }
}

fn same_file(a: &Stat, b: &Stat) -> bool {
    (a.st_dev, a.st_ino) == (b.st_dev, b.st_ino)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn symbolic_links_keep_relative_targets_and_classify_missing_and_looping_targets() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(temp.path().join("текст"), b"unchanged").unwrap();
        let mut progress = Progress::default();
        for (name, target) in [
            ("ok", "../текст"),
            ("missing", "never-existed"),
            ("loop-a", "loop-b"),
            ("loop-b", "loop-a"),
        ] {
            create(
                &folder,
                name,
                Path::new(target),
                Kind::Symbolic,
                &mut progress,
            )
            .unwrap();
            assert_eq!(fs::read_link(folder.join(name)).unwrap(), Path::new(target));
        }
        assert_eq!(fs::read(folder.join("ok")).unwrap(), b"unchanged");
        let entries = super::super::read_directory(&folder, &|| false).unwrap();
        let status = |name| {
            entries
                .iter()
                .find(|entry| entry.name == name)
                .unwrap()
                .metadata
                .link_status
        };
        assert_eq!(status("ok"), Some(LinkStatus::Available));
        assert_eq!(status("missing"), Some(LinkStatus::Missing));
        assert_eq!(status("loop-a"), Some(LinkStatus::Loop));
        for (name, target, expected) in [
            ("ok", "../текст", LinkStatus::Available),
            ("missing", "never-existed", LinkStatus::Missing),
            ("loop-a", "loop-b", LinkStatus::Loop),
        ] {
            let target_details = read_target(&folder.join(name)).unwrap();
            assert_eq!(target_details.path, Path::new(target));
            assert_eq!(target_details.status, expected);
        }
    }

    #[test]
    fn read_target_retains_the_link_path_and_original_errno() {
        use crate::file_error::AccessCause;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing-link");
        let error = read_target(&path).unwrap_err();
        assert_eq!(error.location, Location::Local(path));
        assert_eq!(error.action, AccessAction::ReadLink);
        assert!(matches!(
            error.cause,
            AccessCause::Local(error) if error.raw_os_error() == Some(libc::ENOENT)
        ));
    }

    #[test]
    fn hard_link_keeps_the_inode_and_preserves_existing_paths() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path();
        let source = folder.join("data");
        fs::write(&source, b"untouched").unwrap();
        symlink("missing", folder.join("occupied")).unwrap();
        let mut progress = Progress::default();
        create(
            folder,
            "alias",
            Path::new("data"),
            Kind::Hard,
            &mut progress,
        )
        .unwrap();
        let original = fs::symlink_metadata(&source).unwrap();
        let alias = fs::symlink_metadata(folder.join("alias")).unwrap();
        assert_eq!((original.dev(), original.ino()), (alias.dev(), alias.ino()));
        assert_eq!(alias.nlink(), 2);
        assert!(create(
            folder,
            "occupied",
            Path::new("data"),
            Kind::Hard,
            &mut progress
        )
        .is_err());
        assert!(create(
            folder,
            "occupied",
            Path::new("data"),
            Kind::Symbolic,
            &mut progress
        )
        .is_err());
        assert!(create(
            folder,
            "invalid",
            Path::new("occupied"),
            Kind::Hard,
            &mut progress
        )
        .is_err());
        assert!(create(folder, "invalid", folder, Kind::Hard, &mut progress).is_err());
        assert_eq!(
            fs::read_link(folder.join("occupied")).unwrap(),
            Path::new("missing")
        );
        assert_eq!(fs::read(&source).unwrap(), b"untouched");
        assert_eq!(fs::read_dir(folder).unwrap().count(), 3);
    }

    #[test]
    fn changed_source_and_cancelled_publication_leave_no_hard_link_or_staging() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path();
        let source = folder.join("original");
        let saved = folder.join("saved");
        fs::write(&source, b"original").unwrap();
        let mut progress = Progress::default();
        let mut check = 0;
        let candidate = source.clone();
        let old = saved.clone();
        progress.cancellation().set_checkpoint(move || {
            check += 1;
            if check == 2 {
                fs::rename(&candidate, &old).unwrap();
                fs::write(&candidate, b"replacement").unwrap();
            }
            false
        });
        assert!(create(folder, "alias", &source, Kind::Hard, &mut progress).is_err());

        let mut progress = Progress::default();
        let mut check = 0;
        progress.cancellation().set_checkpoint(move || {
            check += 1;
            check == 3
        });
        let result = create(folder, "cancelled", &source, Kind::Hard, &mut progress);
        assert!(matches!(result, Err(TransferError::Cancelled)));
        assert_eq!(fs::read_dir(folder).unwrap().count(), 2);
        assert_eq!(fs::symlink_metadata(&source).unwrap().nlink(), 1);
        assert_eq!(fs::read(&source).unwrap(), b"replacement");
        assert_eq!(fs::read(&saved).unwrap(), b"original");
    }

    #[test]
    fn hard_link_rejects_a_replaced_destination_and_cleans_its_own_stage() {
        use crate::file_error::Failure;

        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().canonicalize().unwrap().join("directory");
        let held = folder.with_file_name("held");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("original"), b"source").unwrap();
        let mut progress = Progress::default();
        let mut check = 0;
        let requested = folder.clone();
        let moved = held.clone();
        progress.cancellation().set_checkpoint(move || {
            check += 1;
            if check == 2 {
                fs::rename(&requested, &moved).unwrap();
                fs::create_dir(&requested).unwrap();
                fs::write(requested.join("original"), b"replacement").unwrap();
            }
            false
        });

        let result = create(
            &folder,
            "alias",
            Path::new("original"),
            Kind::Hard,
            &mut progress,
        );
        assert!(
            matches!(result, Err(TransferError::Failed(Failure::LinkCreate(error)))
            if matches!(error.as_ref(), LinkCreateError::DirectoryChanged(path) if path == &folder))
        );
        assert_eq!(fs::read_dir(&folder).unwrap().count(), 1);
        assert_eq!(fs::read(folder.join("original")).unwrap(), b"replacement");
        assert_eq!(fs::read_dir(&held).unwrap().count(), 1);
        assert_eq!(fs::read(held.join("original")).unwrap(), b"source");
        assert_eq!(fs::metadata(held.join("original")).unwrap().nlink(), 1);
    }
}
