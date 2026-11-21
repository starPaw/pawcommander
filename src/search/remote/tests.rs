use super::*;
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
            .warning
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
    assert_eq!(cancelled.err().as_deref(), Some(text!("Поиск отменён")));
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
    assert!(expired.warning.contains(text!(
        "Лимит времени поиска SFTP достигнут. Показаны собранные результаты."
    )));
}
