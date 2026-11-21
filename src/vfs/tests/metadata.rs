use super::*;
use crate::result_history::Outcome;
use std::fs::{File, FileTimes, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::time::SystemTime;

const DATA: &[u8] = b"metadata fixture";

fn accessed() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_481_000_000)
}

fn modified() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_501_111_111)
}

fn set_attributes(path: &Path, mode: u32, atime: SystemTime) {
    File::open(path)
        .unwrap()
        .set_times(
            FileTimes::new()
                .set_accessed(atime)
                .set_modified(modified()),
        )
        .unwrap();
    fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
}

fn check_attributes(path: &Path, mode: u32, case: &str) {
    let metadata = fs::symlink_metadata(path).unwrap();
    assert_eq!(
        metadata.permissions().mode() & 0o7777,
        mode,
        "{case} mode {path:?}"
    );
    assert_eq!(
        metadata.accessed().unwrap(),
        accessed(),
        "{case} atime {path:?}"
    );
    assert_eq!(
        metadata.modified().unwrap(),
        modified(),
        "{case} mtime {path:?}"
    );
}

pub(super) fn round_trip(local: &Location, remote: &Location) {
    for (name, source_root, target_root, kind) in [
        ("upload", local, remote, TransferKind::Copy),
        ("download", remote, local, TransferKind::Copy),
        ("remote-copy", remote, remote, TransferKind::Copy),
        ("upload-move", local, remote, TransferKind::Move),
        ("download-move", remote, local, TransferKind::Move),
    ] {
        let source = source_root.join(format!("metadata-{name}-source"));
        let target = target_root.join(format!("metadata-{name}-target"));
        fs::create_dir_all(source.join("nested").path()).unwrap();
        fs::create_dir_all(source.join("empty").path()).unwrap();
        fs::create_dir(target.path()).unwrap();
        fs::write(source.join("nested/file").path(), DATA).unwrap();
        let nodes = [
            ("nested/file", 0o751),
            ("nested", 0o710),
            ("empty", 0o1750),
            ("", 0o750),
        ];
        let entry = |root: &Location, relative: &str| {
            if relative.is_empty() {
                root.clone()
            } else {
                root.join(relative)
            }
        };
        for (path, mode) in nodes {
            set_attributes(entry(&source, path).path(), mode, accessed());
        }
        let mut progress = Progress::default();
        progress.set_copy_verification(true);
        #[cfg(target_os = "macos")]
        if kind == TransferKind::Move {
            progress.resume_directory =
                Some(local.path().join(format!("metadata-{name}-recovery")));
        }
        let report = transfer_with_conflicts(
            std::slice::from_ref(&source),
            &target,
            kind,
            &mut progress,
            |_| panic!("unexpected conflict"),
            |_, _| {},
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(report.completed, 1, "{name}");
        assert_eq!(
            report.metadata_warnings, 0,
            "{name}: {:?}",
            progress.results.items
        );
        let published = target.join(source.name());
        for (path, mode) in nodes {
            check_attributes(entry(&published, path).path(), mode, name);
        }
        assert_eq!(
            fs::read(published.join("nested/file").path()).unwrap(),
            DATA
        );
        assert!(progress
            .results
            .items
            .iter()
            .all(|row| row.outcome == Outcome::Completed));
        assert_eq!(source.path().exists(), kind == TransferKind::Copy);
    }
}

pub(super) fn partially_preserved(local: &Location, remote: &Location) {
    let source = local.join("partial-date");
    fs::write(source.path(), DATA).unwrap();
    let early = UNIX_EPOCH - Duration::from_secs(60 * 60 * 24);
    set_attributes(source.path(), 0o640, early);
    assert_eq!(
        fs::metadata(source.path()).unwrap().accessed().unwrap(),
        early
    );
    let mut progress = Progress::default();
    let report = transfer_with_conflicts(
        std::slice::from_ref(&source),
        remote,
        TransferKind::Copy,
        &mut progress,
        |_| panic!("unexpected conflict"),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(report.completed, 1);
    assert_eq!(report.metadata_warnings, 1);
    let row = &progress.results.items[0];
    assert_eq!(row.outcome, Outcome::CompletedWithWarning);
    let warning = row.metadata_warning.as_deref().unwrap();
    assert!(warning.contains(crate::i18n::text!("время доступа")));
    assert!(!warning.contains(crate::i18n::text!("время изменения")));
    assert!(!warning.contains(crate::i18n::text!("права доступа")));
    let target = remote.join("partial-date");
    let stat = fs::metadata(target.path()).unwrap();
    assert_eq!(stat.modified().unwrap(), modified());
    assert!(stat.accessed().unwrap() > UNIX_EPOCH);
    assert_eq!(stat.permissions().mode() & 0o777, 0o640);
    assert_eq!(fs::read(target.path()).unwrap(), DATA);
}

pub(super) fn cancelled_after_attributes(local: &Location, remote: &Location) {
    let source = local.join("metadata-cancel-source");
    let target = remote.join("metadata-cancel-target");
    fs::create_dir(source.path()).unwrap();
    fs::create_dir(target.path()).unwrap();
    fs::write(source.join("file").path(), DATA).unwrap();
    set_attributes(source.path(), 0o500, accessed());
    let staged_parent = target.path().to_owned();
    let cancel = Cancellation::at_checkpoint(move || {
        fs::read_dir(&staged_parent).unwrap().any(|entry| {
            let entry = entry.unwrap();
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".pawcommander-sftp-")
                && fs::metadata(entry.path().join("payload")).is_ok_and(|metadata| {
                    metadata.is_dir() && metadata.permissions().mode() & 0o777 == 0o500
                })
        })
    });
    let mut progress = Progress::with_cancellation(cancel);
    let report = transfer_with_conflicts(
        std::slice::from_ref(&source),
        &target,
        TransferKind::Copy,
        &mut progress,
        |_| panic!("unexpected conflict"),
        |_, _| {},
    );
    let source_mode = fs::metadata(source.path()).unwrap().permissions().mode() & 0o777;
    fs::set_permissions(source.path(), Permissions::from_mode(0o700)).unwrap();
    let report = report.unwrap();
    assert!(report.cancelled);
    assert_eq!(report.completed, 0);
    assert_eq!(source_mode, 0o500);
    assert_eq!(fs::read(source.join("file").path()).unwrap(), DATA);
    assert!(
        fs::read_dir(target.path()).unwrap().next().is_none(),
        "staging cleanup must traverse the private read-only directory"
    );
}

pub(super) fn server_rejected(local: &Location, remote: &Location) {
    let source = local.join("metadata-rejected-tree");
    fs::create_dir(source.path()).unwrap();
    fs::write(source.join("payload").path(), DATA).unwrap();
    set_attributes(source.join("payload").path(), 0o751, accessed());
    set_attributes(source.path(), 0o750, accessed());
    let mut progress = Progress::default();
    progress.set_copy_verification(true);
    let report = transfer_with_conflicts(
        std::slice::from_ref(&source),
        remote,
        TransferKind::Copy,
        &mut progress,
        |_| panic!("unexpected conflict"),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(report.completed, 1);
    assert_eq!(report.metadata_warnings, 1);
    assert_eq!(progress.results.items.len(), 2);
    for row in progress.results.items {
        assert_eq!(row.outcome, Outcome::CompletedWithWarning);
        let warning = row.metadata_warning.unwrap();
        assert!(warning.contains(crate::i18n::text!("права доступа")));
        assert!(warning.contains(crate::i18n::text!("время доступа")));
        assert!(warning.contains(crate::i18n::text!("время изменения")));
    }
    let target = remote.join(source.name()).join("payload");
    assert_eq!(fs::read(target.path()).unwrap(), DATA);
    assert_eq!(
        fs::metadata(target.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let original = fs::metadata(source.join("payload").path()).unwrap();
    assert_eq!(original.modified().unwrap(), modified());
    assert_eq!(original.permissions().mode() & 0o777, 0o751);
}
