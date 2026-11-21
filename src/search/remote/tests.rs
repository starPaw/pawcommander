use super::*;
use crate::file_error::{Failure, SearchError};
use crate::i18n::text;
use crate::search::{FilterInput, FolderTarget};
use crate::vfs::search::{SftpSearchConnection, Visit, MAX_ENTRIES};
use std::cell::Cell;
use std::fs;
use std::os::unix::fs::symlink;
use std::time::Duration;

pub(crate) fn check_sftp_search(remote_root: &Location) {
    let fixture = tempfile::tempdir_in(remote_root.path()).unwrap();
    let root = remote_root.with_path(fixture.path().to_path_buf());
    let folder = root.join("scope");
    for dir in [
        "scope/nested/deeper",
        "scope/excluded",
        "scope/replaced",
        "peer",
    ] {
        fs::create_dir_all(root.join(dir).path()).unwrap();
    }
    for name in [
        "scope/needle-ТЕСТ.txt",
        "scope/nested/needle-nested.txt",
        "scope/nested/deeper/needle-deep.txt",
        "scope/excluded/needle-excluded.txt",
        "scope/wrong-kind.rs",
        "peer/needle-outside.txt",
    ] {
        fs::write(root.join(name).path(), b"fixture").unwrap();
    }
    symlink(root.join("peer").path(), folder.join("link").path()).unwrap();
    let alias = root.join("alias");
    symlink(folder.path(), alias.path()).unwrap();
    let canonical = folder.with_path(fs::canonicalize(folder.path()).unwrap());

    let deadline = || Instant::now() + Duration::from_secs(DEFAULT_SECONDS);
    let candidates = |scope: &Location, depth| {
        let mut filters = FilterInput::default().parse(FolderTarget::Name).unwrap();
        filters.depth = Some(depth);
        let query =
            FolderSearch::new(FolderTarget::Name, "needle-*.txt", "excluded", filters).unwrap();
        let results = run(scope, &query, deadline(), &|| false).unwrap();
        assert!(results
            .warning()
            .contains(text!("Поиск достиг выбранной глубины вложенности.")));
        results
            .matches
            .into_iter()
            .map(|row| row.path)
            .collect::<Vec<_>>()
    };
    assert_eq!(candidates(&folder, 1), [canonical.join("needle-ТЕСТ.txt")]);
    assert_eq!(
        candidates(&alias, 2),
        [
            canonical.join("needle-ТЕСТ.txt"),
            canonical.join("nested/needle-nested.txt")
        ]
    );

    fs::write(folder.join("needle-small.txt").path(), b"abc").unwrap();
    let filter: FilterInput = serde_json::from_value(serde_json::json!({
        "extension": "txt", "minimum_size": "7", "maximum_size": "7", "depth": "1"
    }))
    .unwrap();
    let query = FolderSearch::new(
        FolderTarget::Name,
        "needle-*",
        "",
        filter.parse(FolderTarget::Name).unwrap(),
    )
    .unwrap();
    let hits = run(&folder, &query, deadline(), &|| false).unwrap().matches;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, canonical.join("needle-ТЕСТ.txt"));

    let replaced = canonical.join("replaced");
    let mut seen = Vec::new();
    let coverage = search::walk(&folder, 4, deadline(), &|| false, |path, stat| {
        if *path == replaced {
            assert!(stat.is_dir());
            fs::remove_dir(path.path()).unwrap();
            symlink(root.join("peer").path(), path.path()).unwrap();
        }
        seen.push(path.clone());
        Visit::Continue
    })
    .unwrap();
    assert_eq!(coverage.unreadable, 1);
    assert!(
        matches!(coverage.failure, Some(TransferError::Failed(Failure::Search(cause)))
        if matches!(cause.as_ref(), SearchError::DirectoryChanged { location, resolved }
            if location == &replaced && resolved.ends_with("peer")))
    );
    assert!(!seen.iter().any(|path| path.name() == "needle-outside.txt"));

    let connection = SftpSearchConnection::open(&folder).unwrap();
    assert_eq!(
        connection
            .directory_at(&folder.join("nested"), true)
            .unwrap(),
        Some(true)
    );
    assert_eq!(
        connection
            .directory_at(&folder.join("needle-ТЕСТ.txt"), false)
            .unwrap(),
        Some(false)
    );
    assert_eq!(
        connection
            .directory_at(&folder.join("gone.txt"), false)
            .unwrap(),
        None
    );
    drop(connection);

    let stopped = Cell::new(false);
    let cancelled = search::walk(&folder, 4, deadline(), &|| stopped.get(), |_, _| {
        assert!(!stopped.replace(true));
        Visit::Continue
    });
    assert!(matches!(cancelled, Err(TransferError::Cancelled)));
    assert!(stopped.get());

    let cutoff = Instant::now() + Duration::from_secs(1);
    let mut visited = 0;
    let coverage = search::walk(&folder, 4, cutoff, &|| false, |_, _| {
        visited += 1;
        std::thread::sleep(
            cutoff.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
        );
        Visit::Continue
    })
    .unwrap();
    assert_eq!(visited, 1);
    assert!(coverage.timed_out);

    let bounded = root.join("entry-limit");
    fs::create_dir(bounded.path()).unwrap();
    for index in 0..MAX_ENTRIES {
        fs::write(bounded.join(format!("sample-{index:05}.txt")).path(), []).unwrap();
    }
    let mut visited = 0;
    let coverage = search::walk(&bounded, 1, deadline(), &|| false, |_, _| {
        visited += 1;
        Visit::Continue
    })
    .unwrap();
    assert!(coverage.entries_limited);
    assert!(!coverage.timed_out);
    assert!((MAX_ENTRIES - 2..=MAX_ENTRIES).contains(&visited));

    let filters = FilterInput::default().parse(FolderTarget::Name).unwrap();
    let query = FolderSearch::new(FolderTarget::Name, "*.txt", "", filters).unwrap();
    let results = run(&bounded, &query, deadline(), &|| false).unwrap();
    assert!(results.limited);
    assert_eq!(results.matches.len(), MAX_RESULTS);
    let expired = run(&bounded, &query, Instant::now(), &|| false).unwrap();
    assert!(expired.matches.is_empty());
    assert!(expired.warning().contains(text!(
        "Лимит времени поиска SFTP достигнут. Показаны собранные результаты."
    )));

    #[cfg(target_os = "macos")]
    check_sftp_search_failures(&root);
}

#[cfg(target_os = "macos")]
fn check_sftp_search_failures(root: &Location) {
    use crate::file_error::{AccessAction, AccessCause, ListingErrorKind};
    use libssh2_sys::LIBSSH2_FX_PERMISSION_DENIED;
    use std::os::unix::fs::PermissionsExt;

    let folder = root.join("search diagnostics");
    fs::create_dir(folder.path()).unwrap();
    let folder = folder.with_path(fs::canonicalize(folder.path()).unwrap());
    let denied = folder.join("unreadable branch");
    fs::create_dir(denied.path()).unwrap();
    let accessible = folder.join("needle.txt");
    let blocked = denied.join("needle-hidden.txt");
    let data = b"keep search fixture bytes";
    fs::write(accessible.path(), data).unwrap();
    fs::write(blocked.path(), data).unwrap();
    let filters = FilterInput::default().parse(FolderTarget::Name).unwrap();
    let query = FolderSearch::new(FolderTarget::Name, "needle", "", filters).unwrap();
    let deadline = || Instant::now() + Duration::from_secs(DEFAULT_SECONDS);
    let permissions = fs::metadata(denied.path()).unwrap().permissions();
    // Traversal can resolve this directory, but OpenSSH cannot list it.
    fs::set_permissions(denied.path(), fs::Permissions::from_mode(0o111)).unwrap();
    let partial = run(&folder, &query, deadline(), &|| false);
    let denied_root = search::walk(&denied, 2, deadline(), &|| false, |_, _| {
        panic!("denied directory yielded a search result")
    });
    fs::set_permissions(denied.path(), permissions.clone()).unwrap();
    let partial = partial.unwrap();
    assert_eq!(partial.matches.len(), 1);
    assert_eq!(partial.matches[0].path, accessible);
    let Warnings::Sftp { coverage, .. } = &partial.warning else {
        panic!("lost typed search coverage");
    };
    assert_eq!(coverage.unreadable, 1);
    let denied_root = denied_root.err().expect("denied root should fail");
    for failure in [coverage.failure.as_ref().unwrap(), &denied_root] {
        assert!(
            matches!(failure, TransferError::Failed(Failure::Listing(error))
            if error.location == denied && matches!(&error.kind, ListingErrorKind::OpenSftp(cause)
                if cause.code() == ssh2::ErrorCode::SFTP(LIBSSH2_FX_PERMISSION_DENIED)))
        );
    }
    assert!(partial.warning().contains(&denied.display()));

    let cycle = folder.join("cyclic scope");
    symlink("cyclic scope", cycle.path()).unwrap();
    let unresolved = search::walk(&cycle, 2, deadline(), &|| false, |_, _| Visit::Continue);
    assert!(
        matches!(unresolved, Err(TransferError::Failed(Failure::Access(error)))
        if error.location == cycle && error.action == AccessAction::ResolvePath
            && matches!(&error.cause, AccessCause::Sftp(cause)
                if matches!(cause.code(), ssh2::ErrorCode::SFTP(_))))
    );

    let connection = SftpSearchConnection::open(&folder).unwrap();
    fs::set_permissions(denied.path(), fs::Permissions::from_mode(0o000)).unwrap();
    let rejected = connection.directory_at(&blocked, true);
    fs::set_permissions(denied.path(), permissions).unwrap();
    assert!(
        matches!(rejected, Err(TransferError::Failed(Failure::Access(error)))
        if error.location == blocked && error.action == AccessAction::CheckSearchResult
            && matches!(&error.cause, AccessCause::Sftp(cause)
                if cause.code() == ssh2::ErrorCode::SFTP(LIBSSH2_FX_PERMISSION_DENIED)))
    );
    assert_eq!(
        connection.directory_at(&blocked, true).unwrap(),
        Some(false)
    );
    assert_eq!(fs::read(blocked.path()).unwrap(), data);
    assert_eq!(fs::read(accessible.path()).unwrap(), data);
}
