use super::*;
#[cfg(unix)]
use crate::file_error::{AccessAction, AccessCause};
use std::{
    fs,
    time::{Duration, UNIX_EPOCH},
};

pub(crate) fn comparison_round_trip(folders: &[Location; 2]) {
    let [left, right] = folders;
    for folder in folders {
        fs::create_dir_all(folder.join("subfolder").path()).unwrap();
    }
    fs::create_dir_all(left.join("one-sided/empty").path()).unwrap();
    fs::create_dir(left.join("type conflict").path()).unwrap();
    fs::write(
        left.join("type conflict/inside").path(),
        b"hidden under conflict",
    )
    .unwrap();
    fs::write(right.join("type conflict").path(), b"regular").unwrap();
    fs::write(left.join("left.txt").path(), b"left").unwrap();
    fs::write(right.join("right.txt").path(), b"right").unwrap();
    let at = UNIX_EPOCH + Duration::new(1_690_000_000, 100_000_000);
    let later = at + Duration::from_secs(30);
    let files: [(&str, &[u8], &[u8]); 4] = [
        ("same.txt", b"xyz", b"xyz"),
        ("time.txt", b"abc", b"abc"),
        ("bytes.txt", b"abc", b"xyz"),
        ("subfolder/size.txt", b"a", b"bb"),
    ];
    for (name, first, second) in files {
        for (side, (folder, contents)) in folders.iter().zip([first, second]).enumerate() {
            let file = folder.join(name);
            let mut output = fs::File::create(file.path()).unwrap();
            std::io::Write::write_all(&mut output, contents).unwrap();
            let modified = if side == 1 && name == "time.txt" {
                later
            } else {
                at
            };
            output
                .set_times(fs::FileTimes::new().set_modified(modified))
                .unwrap();
        }
    }
    let metadata =
        compare_folders(folders, ComparisonMode::Metadata, &mut Progress::default()).unwrap();
    let contents =
        compare_folders(folders, ComparisonMode::Contents, &mut Progress::default()).unwrap();
    for result in [&metadata, &contents] {
        for (name, status) in [
            ("left.txt", ComparisonStatus::LeftOnly),
            ("right.txt", ComparisonStatus::RightOnly),
            ("same.txt", ComparisonStatus::Same),
            ("subfolder/size.txt", ComparisonStatus::Changed),
            ("one-sided", ComparisonStatus::LeftOnly),
            ("one-sided/empty", ComparisonStatus::LeftOnly),
            ("type conflict", ComparisonStatus::TypeChanged),
        ] {
            assert_status(result, name, status);
        }
        assert!(!result
            .rows
            .iter()
            .any(|row| row.path == Path::new("type conflict/inside")));
        assert_eq!(
            (
                result.left_only,
                result.right_only,
                result.changed,
                result.unreadable,
                result.same
            ),
            (3, 1, 3, 0, 2)
        );
        assert_eq!(result.differences.len(), 7);
    }
    assert_status(&metadata, "time.txt", ComparisonStatus::Changed);
    assert_status(&contents, "time.txt", ComparisonStatus::Same);
    assert_status(&metadata, "bytes.txt", ComparisonStatus::Same);
    assert_status(&contents, "bytes.txt", ComparisonStatus::Changed);
    for (name, first, second) in files {
        assert_eq!(fs::read(left.join(name).path()).unwrap(), first);
        assert_eq!(fs::read(right.join(name).path()).unwrap(), second);
    }
}

fn assert_status(result: &Comparison, name: &str, status: ComparisonStatus) {
    let row = result
        .rows
        .iter()
        .find(|row| row.path == Path::new(name))
        .unwrap();
    assert_eq!(
        std::mem::discriminant(&row.status),
        std::mem::discriminant(&status),
        "{name}: {result:?}"
    );
}

#[cfg(unix)]
fn pane_failure<'a>(result: &'a Comparison, name: &str, right: bool) -> &'a TransferError {
    let row = result
        .rows
        .iter()
        .find(|row| row.path == Path::new(name))
        .unwrap();
    let ComparisonStatus::Unreadable(error) = &row.status else {
        panic!("missing read failure for {name}: {row:?}");
    };
    let TransferError::Failed(Failure::ComparisonPane { right: side, cause }) = error.as_ref()
    else {
        panic!("missing pane context for {name}: {error:?}");
    };
    assert_eq!(*side, right);
    cause
}

#[test]
fn directory_comparison_distinguishes_sides_types_timestamps_and_content() {
    let temporary = tempfile::tempdir().unwrap();
    let roots = ["left", "right"].map(|name| {
        let path = temporary.path().join(name);
        fs::create_dir(&path).unwrap();
        Location::Local(path)
    });
    comparison_round_trip(&roots);
}

#[cfg(unix)]
#[test]
fn directory_comparison_reports_unreadable_branches_and_never_traverses_links() {
    let temporary = tempfile::tempdir().unwrap();
    let roots = ["left", "right"].map(|name| {
        let path = temporary.path().join(name);
        fs::create_dir_all(path.join("branch")).unwrap();
        Location::Local(path)
    });
    fs::write(roots[0].join("branch/reachable").path(), b"inside").unwrap();
    let external = temporary.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("secret"), b"outside chosen folders").unwrap();
    for folder in &roots {
        std::os::unix::fs::symlink(&external, folder.join("link").path()).unwrap();
    }
    let view = compare_folders(&roots, ComparisonMode::Contents, &mut Progress::default()).unwrap();
    assert_status(&view, "link", ComparisonStatus::Same);
    assert_status(&view, "branch/reachable", ComparisonStatus::LeftOnly);
    assert!(!view
        .rows
        .iter()
        .any(|row| row.path.starts_with("link") && row.path != Path::new("link")));

    let mut progress = Progress::default();
    let scopes = roots
        .each_ref()
        .map(|folder| ReadScope::open(folder).unwrap());
    let mut scan = Compare {
        scopes: &scopes,
        mode: ComparisonMode::Metadata,
        progress: &mut progress,
        cancel: Cancellation::default(),
        budget: Budget::default(),
        result: Comparison::default(),
    };
    let right_branch = roots[1].join("branch");
    fs::remove_dir(right_branch.path()).unwrap();
    std::os::unix::fs::symlink(external, right_branch.path()).unwrap();
    scan.walk(PathBuf::from("branch"), [true, true], 1).unwrap();
    let [row] = scan.result.rows.as_slice() else {
        panic!("expected one unavailable branch");
    };
    assert_eq!(row.path, Path::new("branch"));
    let cause = pane_failure(&scan.result, "branch", true);
    assert!(matches!(
        cause,
        TransferError::Failed(Failure::Access(error))
            if error.location == scopes[1].parent.location.join("branch")
                && error.action == AccessAction::ReadChildren
                && matches!(&error.cause, AccessCause::Local(cause) if cause.raw_os_error().is_some())
    ));
    assert_eq!(scan.result.left_only, 0);
}

#[cfg(target_os = "macos")]
#[test]
fn content_comparison_keeps_local_open_error_and_other_differences() {
    use std::os::unix::fs::PermissionsExt;

    // SAFETY: geteuid only queries the process identity.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let roots = ["left", "right"].map(|side| {
        let path = temp.path().join(side);
        fs::create_dir(&path).unwrap();
        let path = path.canonicalize().unwrap();
        fs::write(path.join("blocked.bin"), b"same length").unwrap();
        Location::Local(path)
    });
    let blocked = roots[0].join("blocked.bin");
    let permissions = fs::metadata(blocked.path()).unwrap().permissions();
    fs::set_permissions(blocked.path(), fs::Permissions::from_mode(0o000)).unwrap();
    fs::write(roots[1].join("readable.txt").path(), b"readable").unwrap();
    let result = compare_folders(&roots, ComparisonMode::Contents, &mut Progress::default());
    fs::set_permissions(blocked.path(), permissions).unwrap();
    let result = result.unwrap();
    assert_eq!(
        (result.changed, result.right_only, result.unreadable),
        (0, 1, 1)
    );
    assert_status(&result, "readable.txt", ComparisonStatus::RightOnly);
    assert!(matches!(
        pane_failure(&result, "blocked.bin", false),
        TransferError::Failed(Failure::Access(error))
            if error.location == blocked && error.action == AccessAction::OpenFile
                && matches!(&error.cause, AccessCause::Local(cause) if cause.raw_os_error() == Some(libc::EACCES))
    ));
    assert_eq!(fs::read(blocked.path()).unwrap(), b"same length");
}

#[cfg(target_os = "macos")]
pub(crate) fn comparison_sftp_denials(folders: &[Location; 2]) {
    use crate::file_error::ListingErrorKind;
    use std::os::unix::fs::PermissionsExt;

    let fixtures = folders
        .each_ref()
        .map(|root| tempfile::tempdir_in(root.path()).unwrap());
    let roots: [Location; 2] = std::array::from_fn(|side| {
        folders[side].with_path(fixtures[side].path().canonicalize().unwrap())
    });
    for root in &roots {
        fs::create_dir(root.join("blocked").path()).unwrap();
        fs::write(root.join("blocked/hidden.txt").path(), b"inside").unwrap();
        fs::write(root.join("blocked.bin").path(), b"separate").unwrap();
        fs::write(root.join("same.txt").path(), b"same").unwrap();
    }
    fs::write(roots[0].join("readable.txt").path(), b"readable").unwrap();
    let denied = roots[1].join("blocked");
    let blocked = roots[1].join("blocked.bin");
    let permissions = fs::metadata(blocked.path()).unwrap().permissions();
    fs::set_permissions(denied.path(), fs::Permissions::from_mode(0o111)).unwrap();
    fs::set_permissions(blocked.path(), fs::Permissions::from_mode(0o000)).unwrap();
    let result = compare_folders(&roots, ComparisonMode::Contents, &mut Progress::default());
    fs::set_permissions(denied.path(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(blocked.path(), permissions).unwrap();

    let result = result.unwrap();
    assert_eq!(
        (
            result.left_only,
            result.right_only,
            result.changed,
            result.unreadable,
            result.same
        ),
        (1, 0, 0, 2, 1)
    );
    assert_status(&result, "readable.txt", ComparisonStatus::LeftOnly);
    assert!(matches!(
        pane_failure(&result, "blocked", true),
        TransferError::Failed(Failure::Listing(error))
            if error.location == denied && matches!(&error.kind, ListingErrorKind::OpenSftp(cause)
                if cause.code() == ssh2::ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_PERMISSION_DENIED))
    ));
    assert!(matches!(
        pane_failure(&result, "blocked.bin", true),
        TransferError::Failed(Failure::Access(error))
            if error.location == blocked && error.action == AccessAction::OpenFile
                && matches!(&error.cause, AccessCause::Sftp(cause)
                    if cause.code() == ssh2::ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_PERMISSION_DENIED))
    ));
    assert_eq!(
        fs::read(denied.join("hidden.txt").path()).unwrap(),
        b"inside"
    );
    assert_eq!(fs::read(blocked.path()).unwrap(), b"separate");
}

#[test]
fn comparison_counts_owned_diagnostics_against_its_result_limit() {
    let mut result = Comparison {
        path_bytes: MAX_PATH_BYTES - 1,
        ..Comparison::default()
    };
    let cause = ScanError::FolderUnavailable(Location::Local(PathBuf::from("/long-folder-name")));
    let outcome = result.add(
        PathBuf::new(),
        ComparisonStatus::Unreadable(Box::new(cause.into())),
    );
    assert!(matches!(
        outcome,
        Err(TransferError::Failed(Failure::Scan(reason))) if matches!(*reason, ScanError::ComparisonLimit)
    ));
    assert_eq!(result.unreadable, 0);
    assert!(result.rows.is_empty());
}

#[test]
fn directory_comparison_stops_without_returning_an_incomplete_result() {
    let root = tempfile::tempdir().unwrap();
    let location = Location::Local(root.path().into());
    for index in 0..18 {
        fs::write(root.path().join(format!("{index:02}.bin")), b"abc").unwrap();
    }
    let mut checkpoints = 0;
    let cancel = Cancellation::at_checkpoint(move || {
        checkpoints += 1;
        checkpoints == 12
    });
    let mut progress = Progress::with_cancellation(cancel);
    assert!(matches!(
        compare_folders(
            &[location.clone(), location],
            ComparisonMode::Contents,
            &mut progress
        ),
        Err(TransferError::Cancelled)
    ));
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 18);
}
