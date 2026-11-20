use super::*;
use crate::vfs::{sftp, transfer::remote_source_matches};
use std::{
    ffi::OsString,
    fs::{self, File, FileTimes},
    os::unix::ffi::OsStringExt,
    time::{Duration, UNIX_EPOCH},
};

struct Pair {
    _temp: tempfile::TempDir,
    folders: [Location; 2],
    records: PathBuf,
}

impl Pair {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let folders = ["left", "right"].map(|name| {
            let path = temp.path().join(name);
            fs::create_dir(&path).unwrap();
            path.into()
        });
        let records = temp.path().join("records");
        Self {
            _temp: temp,
            folders,
            records,
        }
    }

    fn build(&self) -> TwoWayPlan {
        TwoWayPlan::build_in(
            self.folders.clone(),
            self.records.clone(),
            &mut Progress::default(),
        )
        .unwrap()
    }

    fn file(&self, side: usize, name: &str, contents: &[u8]) {
        let path = self.folders[side].path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
}

fn assert_plan(plan: &TwoWayPlan, expected: &[(&str, Reconcile)]) {
    let rows: BTreeMap<_, _> = plan
        .rows
        .iter()
        .map(|row| (row.path.as_path(), row.outcome))
        .collect();
    let expected: BTreeMap<_, _> = expected
        .iter()
        .map(|(path, outcome)| (Path::new(path), *outcome))
        .collect();
    assert_eq!(rows, expected);
    assert_eq!(
        plan.conflicts,
        expected
            .values()
            .filter(|row| matches!(row, Reconcile::Conflict(_)))
            .count()
    );
}

fn change(destination: usize, change: SyncChange) -> Reconcile {
    Reconcile::Change {
        destination,
        change,
    }
}

#[test]
fn first_comparison_preserves_both_versions_and_only_adds_missing_paths() {
    use Reconcile::Conflict;
    use ReconcileConflict::FirstComparison;
    use SyncChange::Copy;

    let pair = Pair::new();
    pair.file(0, "left-only", b"local");
    pair.file(1, "right-only", b"remote");
    pair.file(0, "different", b"want");
    pair.file(1, "different", b"have");
    let mut plan = pair.build();
    assert!(!plan.has_baseline());
    assert_plan(
        &plan,
        &[
            ("left-only", change(1, Copy)),
            ("right-only", change(0, Copy)),
            ("different", Conflict(FirstComparison)),
        ],
    );
    assert!(plan.remember(&mut Progress::default()).is_err());
    assert!(!pair.records.exists());
}

#[test]
fn common_tree_routes_edits_and_deletions_and_holds_disputed_branches() {
    use Reconcile::Conflict;
    use ReconcileConflict::{BothChanged, EditAndDelete};
    use SyncChange::{Copy, Delete, Replace};

    let pair = Pair::new();
    for side in 0..2 {
        for name in [
            "left-edit",
            "right-edit",
            "both-edit",
            "edit-and-delete",
            "same-edit",
            "removed/sub/file",
            "disputed/file",
        ] {
            pair.file(side, name, b"base");
        }
    }
    let mut first = pair.build();
    first.remember(&mut Progress::default()).unwrap();
    assert!(first.has_baseline());
    for (side, name, data) in [
        (0, "left-edit", b"left"),
        (1, "right-edit", b"peer"),
        (0, "both-edit", b"left"),
        (1, "both-edit", b"peer"),
        (0, "edit-and-delete", b"left"),
        (1, "disputed/file", b"peer"),
        (0, "same-edit", b"same"),
        (1, "same-edit", b"same"),
        (0, "shared/one", b"left"),
        (1, "shared/two", b"peer"),
    ] {
        pair.file(side, name, data);
    }
    fs::remove_dir_all(pair.folders[0].path().join("removed")).unwrap();
    fs::remove_dir_all(pair.folders[0].path().join("disputed")).unwrap();
    fs::remove_file(pair.folders[1].path().join("edit-and-delete")).unwrap();
    let plan = pair.build();
    assert!(plan.has_baseline());
    assert_plan(
        &plan,
        &[
            ("left-edit", change(1, Replace)),
            ("right-edit", change(0, Replace)),
            ("shared/one", change(1, Copy)),
            ("shared/two", change(0, Copy)),
            ("both-edit", Conflict(BothChanged)),
            ("edit-and-delete", Conflict(EditAndDelete)),
            ("disputed", Conflict(EditAndDelete)),
            ("removed", change(1, Delete)),
            ("removed/sub", change(1, Delete)),
            ("removed/sub/file", change(1, Delete)),
        ],
    );
    plan.verify(&mut Progress::default()).unwrap();
    assert_eq!(
        fs::read(pair.folders[1].path().join("removed/sub/file")).unwrap(),
        b"base"
    );
    assert_eq!(
        fs::read(pair.folders[1].path().join("disputed/file")).unwrap(),
        b"peer"
    );
}

#[test]
fn saving_is_stale_checked_and_replaced_folders_do_not_inherit_history() {
    let mut pair = Pair::new();
    pair.file(0, "file", b"same");
    pair.file(1, "file", b"same");
    let mut first = pair.build();
    let mut concurrent = pair.build();
    first.remember(&mut Progress::default()).unwrap();
    assert!(concurrent.remember(&mut Progress::default()).is_err());

    pair.folders.swap(0, 1);
    assert!(pair.build().has_baseline());
    let original = pair.folders[0].path();
    fs::rename(original, pair._temp.path().join("original")).unwrap();
    fs::create_dir(original).unwrap();
    let plan = pair.build();
    assert!(!plan.has_baseline());
    assert_plan(&plan, &[("file", change(0, SyncChange::Copy))]);
}

#[test]
fn common_state_round_trips_raw_names_and_fails_closed_on_damaged_records() {
    let pair = Pair::new();
    let plan = pair.build();
    let tree = Tree::from([(
        PathBuf::from(OsString::from_vec(vec![b'a', 0xff])),
        Kind::Link(OsString::from_vec(vec![b'/', 0xfd])),
    )]);
    let cancel = Progress::default().cancellation();
    plan.store.seed(&tree, &cancel).unwrap();
    assert!(plan.store.read(&cancel).unwrap().as_ref() == Some(&tree));
    let record = fs::read_dir(&pair.records)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(record, b"{").unwrap();
    assert!(plan.store.read(&cancel).is_err());
}

#[test]
fn refuses_a_pair_that_contains_the_state_directory() {
    let pair = Pair::new();
    let inside = pair.folders[0].path().join("PawCommander/states");
    assert!(TwoWayPlan::build_in(pair.folders, inside.clone(), &mut Progress::default()).is_err());
    assert!(!inside.exists());
}

pub(in crate::vfs) fn two_way_round_trip(remote: &Location) {
    let mut pair = Pair::new();
    let fixture = tempfile::tempdir_in(remote.path()).unwrap();
    pair.folders[1] = remote.with_path(fixture.path().to_owned());
    for side in 0..2 {
        pair.file(side, "disputed", b"base");
    }
    let peer = pair.folders[1].join("disputed");
    let time = UNIX_EPOCH + Duration::from_secs(1_650_000_000);
    let set_time = || {
        File::open(peer.path())
            .unwrap()
            .set_times(FileTimes::new().set_modified(time))
            .unwrap()
    };
    set_time();
    let mut seed = pair.build();
    seed.remember(&mut Progress::default()).unwrap();
    let sftp = sftp::remote(&peer).unwrap();
    let before = sftp.lstat(peer.path()).unwrap();

    fs::remove_file(pair.folders[0].path().join("disputed")).unwrap();
    pair.file(1, "disputed", b"peer");
    set_time();
    assert!(remote_source_matches(
        &before,
        &sftp.lstat(peer.path()).unwrap()
    ));
    pair.file(0, "upload", b"left");
    pair.file(1, "download", b"peer");
    pair.folders.swap(0, 1);
    let plan = pair.build();
    assert!(plan.has_baseline());
    assert_plan(
        &plan,
        &[
            ("upload", change(0, SyncChange::Copy)),
            ("download", change(1, SyncChange::Copy)),
            (
                "disputed",
                Reconcile::Conflict(ReconcileConflict::EditAndDelete),
            ),
        ],
    );
    plan.verify(&mut Progress::default()).unwrap();
    assert_eq!(fs::read(peer.path()).unwrap(), b"peer");
    assert!(!fixture.path().join("upload").exists());
}
