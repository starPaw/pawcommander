use super::{
    at, create_staging_directory, directory_path, remove_staging, rename_no_replace, validate_name,
};
use crate::{i18n::text, operations::Progress, vfs::Location};
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::Path;

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

    pub(crate) fn problem(self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::Missing => Some(text!("Цель ссылки отсутствует")),
            Self::Loop => Some(text!("Цикл или слишком длинная цепочка ссылок")),
            Self::Unavailable => Some(text!("Цель ссылки недоступна")),
        }
    }
}

pub(crate) fn describe_target(path: &Path) -> String {
    match fs::read_link(path) {
        Ok(target) => {
            // The kernel resolves relative targets at the link and bounds chain traversal.
            let status = LinkStatus::from_stat(&fs::metadata(path));
            format!(
                "{}: {}\n{}: {}",
                text!("Цель ссылки"),
                target.display(),
                text!("Состояние цели ссылки"),
                status
                    .problem()
                    .unwrap_or_else(|| text!("Цель ссылки доступна")),
            )
        }
        Err(error) => at(text!("Не удалось прочитать цель ссылки"), path, error),
    }
}

pub(crate) fn create(
    directory: &Path,
    name: &str,
    target: &Path,
    kind: Kind,
    progress: &mut Progress,
) -> Result<String, String> {
    validate_name(name)
        .map_err(|_| text!("Введите один компонент имени ссылки без / и \\").to_owned())?;
    if target.as_os_str().is_empty() {
        return Err(text!("Введите путь к цели ссылки").into());
    }
    let directory = directory_path(directory)?;
    let destination = directory.join(name);
    progress.checkpoint().map_err(|error| error.to_string())?;
    let cleanup_warning = match kind {
        Kind::Symbolic => {
            symlink(target, &destination).map_err(|error| create_error(&destination, error))?;
            None
        }
        Kind::Hard => {
            create_hard_link(&directory, &directory.join(target), &destination, progress)?
        }
    };
    let created = crate::tr_format!("Link created: {}", destination.display());
    Ok(match cleanup_warning {
        None => created,
        Some(warning) => format!("{created}. {warning}"),
    })
}

fn create_hard_link(
    directory: &Path,
    source: &Path,
    destination: &Path,
    progress: &mut Progress,
) -> Result<Option<String>, String> {
    let parent = fs::symlink_metadata(directory).map_err(|error| {
        at(
            text!("Не удалось проверить папку назначения"),
            directory,
            error,
        )
    })?;
    let expected = fs::symlink_metadata(source)
        .map_err(|error| at(text!("Не удалось проверить цель ссылки"), source, error))?;
    if !expected.is_file() {
        return Err(text!("Для жёсткой ссылки выберите обычный файл").into());
    }
    if expected.dev() != parent.dev() {
        return Err(text!("Жёсткие ссылки требуют целевой файл на том же томе").into());
    }
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(create_error(
                destination,
                io::ErrorKind::AlreadyExists.into(),
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(create_error(destination, error)),
    }

    progress.checkpoint().map_err(|error| error.to_string())?;
    let staging = create_staging_directory(directory)?;
    let payload = staging.path.join("link");
    let result = (|| {
        let location = Location::Local(staging.path.clone());
        crate::recovery::register_staging(&location).map_err(|error| error.to_string())?;
        staging.ensure_owned().map_err(|error| error.to_string())?;
        // Validate the hard link that the OS actually made in private staging.
        // The source path may have changed since the initial lstat.
        fs::hard_link(source, &payload).map_err(|error| create_error(destination, error))?;
        let made =
            fs::symlink_metadata(&payload).map_err(|error| create_error(destination, error))?;
        if !made.is_file() || !same_file(&made, &expected) {
            return Err(text!("Цель изменилась до создания жёсткой ссылки").into());
        }
        if !fs::symlink_metadata(directory)
            .is_ok_and(|current| current.is_dir() && same_file(&current, &parent))
        {
            return Err(text!("Папка назначения изменилась до создания ссылки").into());
        }
        staging.ensure_owned().map_err(|error| error.to_string())?;
        progress.checkpoint().map_err(|error| error.to_string())?;
        rename_no_replace(&payload, destination).map_err(|error| create_error(destination, error))
    })();
    let warning = remove_staging(&staging).err().map(|error| {
        crate::tr_format!(
            "Could not clean up the temporary folder “{}”: {error}",
            staging.path.display(),
            error = error
        )
    });
    match (result, warning) {
        (Ok(()), warning) => Ok(warning),
        (Err(error), Some(warning)) => Err(format!("{error}. {warning}")),
        (Err(error), None) => Err(error),
    }
}

fn create_error(destination: &Path, error: io::Error) -> String {
    match error.kind() {
        io::ErrorKind::AlreadyExists => crate::tr_format!(
            "“{}” already exists. Choose another name.",
            destination.display()
        ),
        io::ErrorKind::CrossesDevices => {
            text!("Жёсткие ссылки требуют целевой файл на том же томе").into()
        }
        _ => at(text!("Не удалось создать ссылку"), destination, error),
    }
}

fn same_file(a: &Metadata, b: &Metadata) -> bool {
    (a.dev(), a.ino()) == (b.dev(), b.ino())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(describe_target(&folder.join("ok")).contains("../текст"));
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
        assert!(same_file(&original, &alias));
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
        assert!(create(folder, "cancelled", &source, Kind::Hard, &mut progress).is_err());
        assert_eq!(fs::read_dir(folder).unwrap().count(), 2);
        assert_eq!(fs::symlink_metadata(&source).unwrap().nlink(), 1);
        assert_eq!(fs::read(&source).unwrap(), b"replacement");
        assert_eq!(fs::read(&saved).unwrap(), b"original");
    }
}
