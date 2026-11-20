use super::*;
use std::{
    fs,
    os::unix::fs::symlink,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn folders() -> ([tempfile::TempDir; 2], [Location; 2]) {
    let temporary = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let locations = temporary
        .each_ref()
        .map(|folder| folder.path().to_owned().into());
    (temporary, locations)
}

fn rows(plan: &SyncPlan) -> BTreeMap<PathBuf, SyncChange> {
    plan.rows
        .iter()
        .map(|row| (row.path.clone(), row.change))
        .collect()
}

fn set_modified(path: &Path, time: SystemTime) {
    fs::File::open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(time))
        .unwrap();
}

#[test]
fn plan_lists_the_exact_changes_including_a_replaced_directory() {
    let (_temporary, pair) = folders();
    let [source, destination] = &pair;
    fs::create_dir(source.join("new").path()).unwrap();
    fs::write(source.join("new/.hidden").path(), b"new").unwrap();
    fs::write(source.join("type").path(), b"source file").unwrap();
    fs::create_dir(destination.join("type").path()).unwrap();
    fs::write(destination.join("type/old").path(), b"old subtree").unwrap();
    fs::write(destination.join("extra").path(), b"keep by default").unwrap();
    fs::write(source.join("changed").path(), b"new!").unwrap();
    fs::write(destination.join("changed").path(), b"old!").unwrap();
    fs::write(source.join("same").path(), b"same").unwrap();
    fs::write(destination.join("same").path(), b"same").unwrap();
    let time = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    set_modified(source.join("changed").path(), time);
    set_modified(destination.join("changed").path(), time);
    set_modified(destination.join("same").path(), time);
    symlink("new/.hidden", source.join("link").path()).unwrap();
    symlink("missing", destination.join("link").path()).unwrap();

    let mut progress = Progress::default();
    let plan = SyncPlan::build(pair.clone(), false, &mut progress).unwrap();
    assert_eq!(
        rows(&plan),
        [
            ("changed", SyncChange::Replace),
            ("link", SyncChange::Replace),
            ("new", SyncChange::Copy),
            ("new/.hidden", SyncChange::Copy),
            ("type", SyncChange::Replace),
            ("type/old", SyncChange::Delete),
        ]
        .map(|(path, change)| (PathBuf::from(path), change))
        .into()
    );
    assert_eq!((plan.copies, plan.replacements, plan.deletions), (2, 3, 1));
    plan.verify(&mut progress).unwrap();
    assert_eq!(
        fs::read(destination.join("type/old").path()).unwrap(),
        b"old subtree"
    );
    assert!(!destination.join("new").path().exists());
    assert_eq!(
        fs::read_link(destination.join("link").path()).unwrap(),
        Path::new("missing")
    );

    let mirror = SyncPlan::build(pair, true, &mut progress).unwrap();
    assert_eq!(rows(&mirror)[Path::new("extra")], SyncChange::Delete);
    assert_eq!(mirror.deletions, 2);
}

#[test]
fn plan_rejects_nested_or_aliased_folders() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("folder");
    let nested = folder.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let alias = root.path().join("alias");
    symlink(&folder, &alias).unwrap();
    let error = SyncPlan::build(
        [alias.into(), nested.into()],
        true,
        &mut Progress::default(),
    )
    .err()
    .unwrap();
    assert_eq!(
        error.to_string(),
        text("Источник и назначение синхронизации пересекаются")
    );
}

#[test]
fn verification_rejects_new_items_even_when_they_were_not_in_the_plan() {
    let (_temporary, pair) = folders();
    let mut progress = Progress::default();
    let plan = SyncPlan::build(pair.clone(), false, &mut progress).unwrap();
    fs::write(pair[1].join("new-extra").path(), b"unexpected").unwrap();
    assert!(plan.verify(&mut progress).is_err());
    assert_eq!(
        fs::read(pair[1].join("new-extra").path()).unwrap(),
        b"unexpected"
    );
}

pub(in crate::vfs) fn sync_round_trip(remote: &Location) {
    use super::super::{sftp, transfer::remote_source_matches};

    let local = tempfile::tempdir().unwrap();
    let remote_fixture = tempfile::tempdir_in(remote.path()).unwrap();
    let source: Location = local.path().to_owned().into();
    let destination = remote.with_path(remote_fixture.path().to_owned());
    let peer = destination.join("changed.bin");
    fs::write(source.join("changed.bin").path(), b"want").unwrap();
    fs::write(source.join(".new").path(), b"copy").unwrap();
    fs::write(peer.path(), b"have").unwrap();
    fs::write(destination.join("extra").path(), b"extra").unwrap();
    let time = UNIX_EPOCH + Duration::from_secs(1_650_000_000);
    set_modified(peer.path(), time);
    let mut progress = Progress::default();
    let plan = SyncPlan::build([source.clone(), destination.clone()], true, &mut progress).unwrap();
    assert_eq!((plan.copies, plan.replacements, plan.deletions), (1, 1, 1));
    plan.verify(&mut progress).unwrap();
    let channel = sftp::remote(&peer).unwrap();
    let original = channel.lstat(peer.path()).unwrap();
    fs::write(peer.path(), b"edit").unwrap();
    set_modified(peer.path(), time);
    let edited = channel.lstat(peer.path()).unwrap();
    assert!(remote_source_matches(&original, &edited));
    let error = plan.verify(&mut progress).unwrap_err();
    assert!(error.to_string().contains("changed.bin"));
    assert_eq!(fs::read(peer.path()).unwrap(), b"edit");
    assert!(destination.join("extra").path().is_file());
    assert!(!destination.join(".new").path().exists());

    let reverse = SyncPlan::build([destination, source], false, &mut progress).unwrap();
    assert_eq!(
        (reverse.copies, reverse.replacements, reverse.deletions),
        (1, 1, 0)
    );
}
