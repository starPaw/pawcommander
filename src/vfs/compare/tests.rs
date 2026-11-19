use super::*;
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
    let row = result.rows.iter().find(|row| row.path == Path::new(name));
    assert_eq!(
        row.map(|row| &row.status),
        Some(&status),
        "{name}: {result:?}"
    );
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
    assert!(matches!(row.status, ComparisonStatus::Unreadable(_)));
    assert_eq!(scan.result.left_only, 0);
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
