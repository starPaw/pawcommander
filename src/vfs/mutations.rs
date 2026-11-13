use super::location::{normalize_remote, validate_name, Location};
use super::sftp::remote;
use super::{err, MAX_TREE_DEPTH};
use crate::operations::Progress;
use crate::{archive, fs_ops};
use ssh2::{RenameFlags, Sftp};

pub fn create_directory(parent: &Location, name: &str) -> Result<Location, String> {
    validate_name(name)?;
    if let Some(path) = parent.local() {
        return fs_ops::create_directory(path, name).map(Location::Local);
    }
    let target = parent.join(name);
    remote(parent)?.mkdir(target.path(), 0o755).map_err(err)?;
    Ok(target)
}

pub fn rename_entry(source: &Location, name: &str) -> Result<Location, String> {
    validate_name(name)?;
    if let Some(path) = source.local() {
        return fs_ops::rename_entry(path, name).map(Location::Local);
    }
    let target = source
        .parent()
        .ok_or("Нельзя переименовать корень")?
        .join(name);
    if &target != source {
        remote(source)?
            .rename(source.path(), target.path(), Some(RenameFlags::empty()))
            .map_err(err)?;
    }
    Ok(target)
}

pub(super) fn remove_remote_tree(
    sftp: &Sftp,
    location: &Location,
    remaining_depth: usize,
    progress: &mut Progress,
    journal: bool,
) -> Result<(), String> {
    if journal {
        progress.results.removing(location);
    }
    let path = location.path();
    let result = (|| {
        progress.item(path);
        let stat = sftp.lstat(path).map_err(err)?;
        if stat.is_dir() {
            for (child, _) in sftp.readdir(path).map_err(err)? {
                let child_depth = remaining_depth
                    .checked_sub(1)
                    .ok_or("Слишком большая глубина папок")?;
                let name = child
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or("Некорректное имя SFTP")?;
                validate_name(name)?;
                remove_remote_tree(sftp, &location.join(name), child_depth, progress, journal)?;
            }
            progress.item(path);
            sftp.rmdir(path).map_err(err)
        } else {
            sftp.unlink(path).map_err(err)
        }
    })();
    if journal {
        progress.results.removed(location, &result);
    }
    result
}

#[cfg(test)]
pub fn trash_entries(paths: &[Location]) -> Result<usize, String> {
    trash_entries_with_progress(paths, &mut Progress::default())
}

pub fn trash_entries_with_progress(
    paths: &[Location],
    progress: &mut Progress,
) -> Result<usize, String> {
    progress
        .results
        .plan(paths.iter().map(|path| (path.clone(), None)));
    if paths.iter().any(Location::is_archive) {
        return Err(archive::readonly().into());
    }
    if paths.iter().any(Location::remote) {
        if paths.iter().any(|p| {
            !p.remote()
                || p.parent().is_none()
                || p.path() != normalize_remote(p.path()).unwrap_or_default()
        }) {
            return Err("Недопустимый путь удаления SFTP".into());
        }
        for (completed, path) in paths.iter().enumerate() {
            progress.results.begin(path);
            progress.item(path.path());
            remove_remote_tree(&remote(path)?, path, MAX_TREE_DEPTH, progress, true).map_err(|e| format!("{e}. Удалено целых элементов: {completed}; текущая папка могла быть удалена частично."))?;
            progress
                .results
                .record(path, None, crate::result_history::Outcome::Completed, None);
        }
        return Ok(paths.len());
    }
    fs_ops::trash_entries(
        &paths.iter().map(|p| p.path().into()).collect::<Vec<_>>(),
        progress,
    )
}
