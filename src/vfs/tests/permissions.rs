use crate::file_error::{AccessAction, AccessCause, Failure, PermissionError};
use crate::i18n::{file_error, text};
use crate::operations::{Cancellation, Progress, TransferError};
use crate::result_history::Outcome;
use crate::vfs::{Location, ModeRule, PermissionsPreview};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};

fn mode(path: &Location) -> u32 {
    fs::metadata(path.path()).unwrap().permissions().mode() & 0o7777
}

fn set_mode(path: &Location, mode: u32) {
    fs::set_permissions(path.path(), fs::Permissions::from_mode(mode)).unwrap();
}

pub(super) fn check(remote_root: &Location, server_denies: bool) {
    let fixture = tempfile::tempdir_in(remote_root.path()).unwrap();
    let directory = remote_root.with_path(fixture.path().canonicalize().unwrap());
    let file = directory.join("file.txt");
    fs::write(file.path(), b"payload").unwrap();
    set_mode(&file, 0o444);

    let cancel = Cancellation::default();
    let numeric = ModeRule::parse("0600").unwrap();
    if server_denies {
        let plan =
            PermissionsPreview::load(std::slice::from_ref(&file), &numeric, &cancel).unwrap();
        let mut progress = Progress::default();
        let reason = text!("Сервер SFTP отказал в доступе.");
        let report = plan.apply(&mut progress).unwrap();
        assert_eq!((report.completed, report.failed), (0, 1));
        let error = report.error.unwrap();
        assert!(matches!(
            &error,
            TransferError::Failed(Failure::Permissions(error))
                if matches!(error.as_ref(), PermissionError::SetMode { location, source }
                    if location == &file && source.code() ==
                        ssh2::ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_PERMISSION_DENIED))
        ));
        assert_eq!(progress.results.items[0].outcome, Outcome::Failed);
        let detail = file_error(&error);
        assert!(detail.contains(reason) && detail.contains(&file.display()));
        assert_eq!(
            progress.results.items[0].reason.as_deref(),
            Some(detail.as_str())
        );
        assert_eq!(mode(&file), 0o444);
        assert_eq!(fs::read(file.path()).unwrap(), b"payload");
        return;
    }

    let subdir = directory.join("folder");
    let child = subdir.join("child.txt");
    let unchanged = directory.join("unchanged.txt");
    let stale = directory.join("stale.txt");
    let link = directory.join("link.txt");
    let missing = directory.join("missing.txt");
    fs::create_dir(subdir.path()).unwrap();
    set_mode(&subdir, 0o755);
    for (path, bits) in [(&child, 0o604), (&unchanged, 0o640), (&stale, 0o764)] {
        fs::write(path.path(), b"sample").unwrap();
        set_mode(path, bits);
    }
    symlink(file.path(), link.path()).unwrap();
    let sources = [
        file.clone(),
        subdir.clone(),
        unchanged,
        stale.clone(),
        link.clone(),
        missing.clone(),
    ];
    let symbolic = ModeRule::parse("u+rw,g-w,o=").unwrap();
    let plan = PermissionsPreview::load(&sources, &symbolic, &cancel).unwrap();
    assert_eq!(plan.changes, 3);
    assert_eq!(
        plan.rows
            .iter()
            .map(|row| row
                .change
                .as_ref()
                .ok()
                .map(|change| (change.current(), change.after)))
            .collect::<Vec<_>>(),
        [
            Some((0o444, 0o640)),
            Some((0o755, 0o750)),
            Some((0o640, 0o640)),
            Some((0o764, 0o740)),
            None,
            None
        ],
    );
    assert!(matches!(
        &plan.rows[4].change,
        Err(TransferError::Failed(Failure::Permissions(error)))
            if matches!(error.as_ref(), PermissionError::UnsupportedType(location) if location == &link)
    ));
    assert!(matches!(
        &plan.rows[5].change,
        Err(TransferError::Failed(Failure::Access(error)))
            if error.location == missing && error.action == AccessAction::ReadMetadata
                && matches!(&error.cause, AccessCause::Sftp(source)
                    if source.code() == ssh2::ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_NO_SUCH_FILE))
    ));
    assert_eq!(mode(&file), 0o444);
    assert_eq!(mode(&subdir), 0o755);

    fs::write(stale.path(), b"independent edit after preview").unwrap();
    let mut progress = Progress::default();
    let report = plan.apply(&mut progress).unwrap();
    assert_eq!(
        (
            report.completed,
            report.warned,
            report.skipped,
            report.failed
        ),
        (2, 0, 3, 1)
    );
    assert!(matches!(
        report.error,
        Some(TransferError::Failed(Failure::Permissions(error)))
            if matches!(error.as_ref(), PermissionError::ItemChanged(location) if location == &stale)
    ));
    assert_eq!(
        progress
            .results
            .items
            .iter()
            .map(|row| row.outcome)
            .collect::<Vec<_>>(),
        [
            Outcome::Completed,
            Outcome::Completed,
            Outcome::Skipped,
            Outcome::Failed,
            Outcome::Skipped,
            Outcome::Skipped,
        ]
    );
    assert_eq!(mode(&file), 0o640);
    assert_eq!(mode(&subdir), 0o750);
    assert_eq!(mode(&child), 0o604);
    assert_eq!(mode(&stale), 0o764);
    assert_eq!(
        fs::read(stale.path()).unwrap(),
        b"independent edit after preview"
    );
    assert_eq!(fs::read(file.path()).unwrap(), b"payload");

    let alternate = directory.join("alternate");
    let alias = directory.join("alias");
    fs::create_dir(alternate.path()).unwrap();
    let alternate_child = alternate.join("child.txt");
    fs::write(alternate_child.path(), b"elsewhere").unwrap();
    set_mode(&alternate_child, 0o664);
    symlink(subdir.path(), alias.path()).unwrap();
    let selected = alias.join("child.txt");
    let plan =
        PermissionsPreview::load(std::slice::from_ref(&selected), &numeric, &cancel).unwrap();
    fs::remove_file(alias.path()).unwrap();
    symlink(alternate.path(), alias.path()).unwrap();
    let mut progress = Progress::default();
    assert!(matches!(
        plan.apply(&mut progress),
        Err(TransferError::Failed(Failure::Permissions(error)))
            if matches!(error.as_ref(), PermissionError::FolderChanged(location) if location == &alias)
    ));
    assert_eq!(
        progress.results.root_outcomes(),
        [(selected, Outcome::NotStarted)]
    );
    assert_eq!(mode(&child), 0o604);
    assert_eq!(mode(&alternate_child), 0o664);

    let plan = PermissionsPreview::load(std::slice::from_ref(&file), &numeric, &cancel).unwrap();
    let mut checks = 0;
    let mut stopped = Progress::with_cancellation(Cancellation::at_checkpoint(move || {
        checks += 1;
        // Open the session, resolve the directory and begin the row, then
        // cancel after the final metadata check and before SETSTAT.
        checks == 5
    }));
    let report = plan.apply(&mut stopped).unwrap();
    assert!(report.cancelled && report.error.is_none());
    assert!(stopped.results.cancelled);
    assert_eq!(stopped.results.items[0].outcome, Outcome::Cancelled);
    assert_eq!(mode(&file), 0o640);

    let plan = PermissionsPreview::load(std::slice::from_ref(&file), &numeric, &cancel).unwrap();
    let report = plan.apply(&mut Progress::default()).unwrap();
    assert_eq!(report.completed, 1);
    assert!(report.error.is_none());
    assert_eq!(mode(&file), 0o600);
}
