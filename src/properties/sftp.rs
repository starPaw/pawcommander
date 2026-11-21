use super::{date, Summary};
use crate::{
    i18n::text,
    vfs::{visit_properties, Location, PropertyEvent},
};
use ssh2::{FileStat, FileType};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, UNIX_EPOCH},
};

pub(super) fn scan(
    sources: &[Location],
    cancel: &AtomicBool,
    mut progress: impl FnMut(&Summary),
) -> Summary {
    let mut summary = Summary::default();
    visit_properties(sources, cancel, |event| {
        match event {
            PropertyEvent::Entry {
                location,
                stat,
                selected,
            } => {
                match stat.file_type() {
                    FileType::Directory => summary.directories += 1,
                    FileType::Symlink => summary.links += 1,
                    FileType::RegularFile => {
                        summary.files += 1;
                        if let Some(size) = stat.size {
                            summary.bytes = summary.bytes.saturating_add(size);
                        } else {
                            summary
                                .read_error(location, text!("Сервер SFTP не сообщил размер файла"));
                        }
                    }
                    FileType::Other(0) => {
                        summary.read_error(location, text!("Сервер SFTP не сообщил тип объекта"));
                    }
                    _ => summary.other += 1,
                }
                if selected {
                    summary.details.push((location.clone(), details(stat)));
                }
            }
            PropertyEvent::Error(location, error) => {
                summary.read_error(location, crate::i18n::file_error(&error));
            }
        }
        progress(&summary);
    });
    summary.cancelled = cancel.load(Ordering::Relaxed);
    summary.done = true;
    summary
}

fn details(stat: &FileStat) -> String {
    let unavailable = || text!("недоступно").to_owned();
    let mode = stat
        .perm
        .map_or_else(unavailable, |mode| format!("{:04o}", mode & 0o7777));
    let owner = stat.uid.map_or_else(unavailable, |uid| uid.to_string());
    let group = stat.gid.map_or_else(unavailable, |gid| gid.to_string());
    let permissions = format!("{mode} · UID {owner} · GID {group}");
    let remote_date = |seconds: Option<u64>| {
        date(seconds.and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds))))
    };
    crate::tr_format!(
        "Permissions: {permissions}\nModified: {}\nCreated: {}\nAccessed: {}",
        remote_date(stat.mtime),
        text!("недоступно"),
        remote_date(stat.atime),
        permissions = permissions
    )
}

#[cfg(test)]
pub(crate) fn check_sftp_properties(remote: &Location) {
    use std::{fs, os::unix::fs::PermissionsExt, slice::from_ref};

    let fixture = tempfile::tempdir_in(remote.path()).unwrap();
    let root = remote.with_path(fixture.path().to_owned());
    let tree = root.join("tree");
    let sub = tree.join("sub");
    let outside = root.join("outside");
    let peer = root.join("peer");
    let link = root.join("selected-link");
    fs::create_dir_all(sub.path()).unwrap();
    fs::create_dir(outside.path()).unwrap();
    fs::write(outside.join("target").path(), b"outside bytes").unwrap();
    fs::write(tree.join(".hidden").path(), b"hidden").unwrap();
    fs::write(sub.join("payload").path(), b"payload").unwrap();
    fs::hard_link(sub.join("payload").path(), tree.join("hard-link").path()).unwrap();
    fs::write(peer.path(), b"data").unwrap();
    fs::set_permissions(peer.path(), fs::Permissions::from_mode(0o640)).unwrap();
    let modified = UNIX_EPOCH + Duration::from_secs(1_650_000_000);
    let accessed = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    fs::File::open(peer.path())
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_modified(modified)
                .set_accessed(accessed),
        )
        .unwrap();
    std::os::unix::fs::symlink(tree.path(), sub.join("loop").path()).unwrap();
    std::os::unix::fs::symlink(outside.path(), tree.join("outside-link").path()).unwrap();
    std::os::unix::fs::symlink(root.join("missing").path(), tree.join("broken").path()).unwrap();
    std::os::unix::fs::symlink(outside.path(), link.path()).unwrap();

    let cancel = AtomicBool::new(false);
    let result = scan(
        &[tree.clone(), peer.clone(), tree.clone(), sub, link],
        &cancel,
        |_| {},
    );
    assert!(result.done && !result.cancelled);
    assert_eq!(result.errors, 0, "{result:?}");
    assert_eq!(
        (
            result.files,
            result.directories,
            result.links,
            result.other,
            result.bytes
        ),
        (4, 2, 4, 0, 24)
    );
    assert_eq!(result.details.len(), 3);
    let (_, details) = result
        .details
        .iter()
        .find(|(path, _)| *path == peer)
        .unwrap();
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(peer.path()).unwrap();
    assert!(details.contains(&format!(
        "0640 · UID {} · GID {}",
        metadata.uid(),
        metadata.gid()
    )));
    assert!(details.contains(&date(Some(modified))));
    assert!(details.contains(&date(Some(accessed))));
    assert!(details.contains(text!("недоступно")));

    let cancel_root = root.join("cancellation");
    let queued = cancel_root.join("child");
    fs::create_dir_all(queued.path()).unwrap();
    fs::write(queued.join("unread").path(), b"unread").unwrap();
    let result = scan(from_ref(&cancel_root), &cancel, |summary| {
        if summary.directories == 2 {
            cancel.store(true, Ordering::Relaxed);
        }
    });
    assert!(result.done && result.cancelled);
    assert_eq!(
        (
            result.files,
            result.directories,
            result.bytes,
            result.errors
        ),
        (0, 2, 0, 0)
    );

    cancel.store(false, Ordering::Relaxed);
    let changing = root.join("changing");
    let branch = changing.join("branch");
    fs::create_dir_all(branch.path()).unwrap();
    fs::write(branch.join("original").path(), b"original").unwrap();
    let mut replaced = false;
    let result = scan(&[changing, peer], &cancel, |summary| {
        if summary.directories == 2 && !replaced {
            fs::rename(branch.path(), root.join("former-branch").path()).unwrap();
            std::os::unix::fs::symlink(outside.path(), branch.path()).unwrap();
            replaced = true;
        }
    });
    assert!(replaced && result.done && !result.cancelled);
    assert_eq!((result.files, result.bytes, result.errors), (1, 4, 1));
    assert!(result.error_examples[0].contains(&branch.name()));
}
