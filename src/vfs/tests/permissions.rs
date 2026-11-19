use crate::i18n::text;
use crate::operations::{Cancellation, Progress};
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
    let directory = remote_root.with_path(fixture.path().to_path_buf());
    let file = directory.join("file.txt");
    fs::write(file.path(), b"payload").unwrap();
    set_mode(&file, 0o444);

    let cancel = Cancellation::default();
    let numeric = ModeRule::parse("0600").unwrap();
    if server_denies {
        let plan =
            PermissionsPreview::load(std::slice::from_ref(&file), &numeric, &cancel).unwrap();
        let mut progress = Progress::default();
        let reason = text("Сервер SFTP отказал в доступе.");
        assert!(plan.apply(&mut progress).unwrap_err().contains(reason));
        assert_eq!(progress.results.items[0].outcome, Outcome::Failed);
        assert_eq!(progress.results.items[0].reason.as_deref(), Some(reason));
        assert_eq!(mode(&file), 0o444);
        return;
    }

    let subdir = directory.join("folder");
    let child = subdir.join("child.txt");
    let unchanged = directory.join("unchanged.txt");
    let stale = directory.join("stale.txt");
    let link = directory.join("link.txt");
    fs::create_dir(subdir.path()).unwrap();
    set_mode(&subdir, 0o755);
    for (path, bits) in [(&child, 0o604), (&unchanged, 0o640), (&stale, 0o764)] {
        fs::write(path.path(), b"sample").unwrap();
        set_mode(path, bits);
    }
    symlink(file.path(), link.path()).unwrap();
    let sources = [file.clone(), subdir.clone(), unchanged, stale.clone(), link];
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
            None
        ],
    );
    assert_eq!(mode(&file), 0o444);
    assert_eq!(mode(&subdir), 0o755);

    fs::write(stale.path(), b"independent edit after preview").unwrap();
    let mut progress = Progress::default();
    let stale_reason = text("Объект SFTP изменился. Обновите предпросмотр прав.");
    assert!(plan
        .apply(&mut progress)
        .unwrap_err()
        .contains(stale_reason));
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
        ]
    );
    assert_eq!(mode(&file), 0o640);
    assert_eq!(mode(&subdir), 0o750);
    assert_eq!(mode(&child), 0o604);
    assert_eq!(mode(&stale), 0o764);
    assert_eq!(fs::read(file.path()).unwrap(), b"payload");

    let plan = PermissionsPreview::load(std::slice::from_ref(&file), &numeric, &cancel).unwrap();
    let mut checks = 0;
    let mut stopped = Progress::with_cancellation(Cancellation::at_checkpoint(move || {
        checks += 1;
        checks == 3
    }));
    plan.apply(&mut stopped).unwrap();
    assert!(stopped.results.cancelled);
    assert_eq!(stopped.results.items[0].outcome, Outcome::Cancelled);
    assert_eq!(mode(&file), 0o640);

    let plan = PermissionsPreview::load(std::slice::from_ref(&file), &numeric, &cancel).unwrap();
    plan.apply(&mut Progress::default()).unwrap();
    assert_eq!(mode(&file), 0o600);
}
