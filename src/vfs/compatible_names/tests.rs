use super::*;
use std::ffi::OsStr;

#[test]
fn repairs_reserved_names_length_and_unicode_without_extending_over_limits() {
    let proposed = Name::suggest(OsStr::new("cafe\u{301}:plan?.txt. "), 255, false).unwrap();
    assert_eq!(proposed.value, "café_plan_.txt");
    assert!(proposed.reasons.contains(&Reason::Characters));
    assert!(proposed.reasons.contains(&Reason::Trailing));
    assert!(proposed.reasons.contains(&Reason::Unicode));
    let device = Name::suggest(OsStr::new("COM¹.tar.gz"), 255, false).unwrap();
    assert_eq!(device.value, "_COM¹.tar.gz");
    assert_eq!(device.reasons, [Reason::Reserved]);
    let native = Name::suggest(OsStr::from_bytes(b"raw\xff.txt"), 255, false).unwrap();
    assert_eq!(native.value, "raw\u{fffd}.txt");
    assert_eq!(native.reasons, [Reason::Encoding]);

    let long = format!("{}.md", "🙂".repeat(128));
    let first = Name::suggest(OsStr::new(&long), 34, false).unwrap();
    let mut occupied = HashSet::from([portable_key(OsStr::new(&first.value))]);
    let second = first
        .reserve(34, false, &mut occupied, |_| Ok(false))
        .unwrap();
    assert!(second.value.len() <= 34);
    assert!(second.value.ends_with(" (2).md"));
    assert_eq!(second.reasons, [Reason::Length, Reason::Collision]);
    let utf16 = Name::suggest(OsStr::new(&long), 1024, false).unwrap();
    assert!(utf16.value.encode_utf16().count() <= 255);
    assert!(utf16.value.ends_with(".md"));
    assert_eq!(
        portable_key(OsStr::new("Café")),
        portable_key(OsStr::new("cafe\u{301}"))
    );
    assert_eq!(
        portable_key(OsStr::new("Straße")),
        portable_key(OsStr::new("STRAẞE"))
    );
}

#[test]
fn fat_names_protect_existing_metadata_paths_and_fit_in_decomposed_form() {
    let mut occupied = HashSet::from([portable_key(OsStr::new("._CAFÉ.TXT"))]);
    let suggestion = Name::suggest(OsStr::new("café.txt"), 24, true)
        .unwrap()
        .reserve(24, true, &mut occupied, |_| Ok(false))
        .unwrap();
    assert_eq!(suggestion.value, "café (2).txt");
    assert_eq!(suggestion.reasons, [Reason::Collision]);
    assert!(occupied.contains(&portable_key(OsStr::new("._café (2).txt"))));

    let explicit = Name::suggest(OsStr::new("._notes"), 24, true).unwrap();
    assert_eq!(explicit.value, "__notes");
    assert_eq!(explicit.reasons, [Reason::MetadataName]);

    let long = Name::suggest(OsStr::new("ééé.txt"), 11, true).unwrap();
    assert_eq!(long.value, "éé.txt");
    assert_eq!(long.reasons, [Reason::Length]);
}

fn folders() -> (tempfile::TempDir, Location, Location) {
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let source = Location::Local(root.path().join("left"));
    let target = Location::Local(root.path().join("right"));
    fs::create_dir(source.path()).unwrap();
    fs::create_dir(target.path()).unwrap();
    (root, source, target)
}

#[test]
fn copy_applies_the_reviewed_nested_names_and_preserves_existing_items() {
    let (_root, source, target) = folders();
    let bundle = source.join("Reports");
    fs::create_dir(bundle.path()).unwrap();
    fs::write(bundle.join("a:b.txt").path(), b"changed-name").unwrap();
    fs::write(bundle.join("a_b.txt").path(), b"keep-spelling").unwrap();
    fs::write(target.join("reports").path(), b"occupied by casing").unwrap();
    let plan = build(
        std::slice::from_ref(&bundle),
        &target,
        TransferKind::Copy,
        &mut Progress::default(),
    )
    .unwrap();
    assert_eq!(plan.entries, 3);
    assert_eq!(plan.changes.len(), 2);
    assert_eq!(plan.changes[0].after, Path::new("Reports (2)"));
    assert_eq!(plan.changes[1].after, Path::new("Reports (2)/a_b (2).txt"));
    assert!(!target.join("Reports (2)").path().exists());
    plan.apply(&mut Progress::default()).unwrap();
    let output = target.join("Reports (2)");
    assert_eq!(
        fs::read(output.join("a_b.txt").path()).unwrap(),
        b"keep-spelling"
    );
    assert_eq!(
        fs::read(output.join("a_b (2).txt").path()).unwrap(),
        b"changed-name"
    );
    assert_eq!(
        fs::read(bundle.join("a:b.txt").path()).unwrap(),
        b"changed-name"
    );
    assert_eq!(
        fs::read(target.join("reports").path()).unwrap(),
        b"occupied by casing"
    );
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 2);
}

#[test]
fn move_verifies_output_before_removing_the_tree_with_its_original_names() {
    let (_root, source, target) = folders();
    let folder = source.join("AUX");
    fs::create_dir_all(folder.join("old:reports").path()).unwrap();
    fs::write(folder.join("old:reports/LPT3.txt").path(), b"verified file").unwrap();
    let plan = build(
        std::slice::from_ref(&folder),
        &target,
        TransferKind::Move,
        &mut Progress::default(),
    )
    .unwrap();
    assert_eq!(plan.changes.len(), 3);
    plan.apply(&mut Progress::default()).unwrap();
    assert_eq!(
        fs::read(target.join("_AUX/old_reports/_LPT3.txt").path()).unwrap(),
        b"verified file"
    );
    assert!(!folder.path().exists());
    assert_eq!(fs::read_dir(source.path()).unwrap().count(), 0);
}

#[test]
fn a_stale_source_or_new_destination_invalidates_the_entire_reviewed_plan() {
    for source_changed in [true, false] {
        let (_root, source, target) = folders();
        let first = source.join("first.txt");
        let second = source.join("second.txt");
        fs::write(first.path(), b"first").unwrap();
        fs::write(second.path(), b"second").unwrap();
        let plan = build(
            &[first.clone(), second.clone()],
            &target,
            TransferKind::Move,
            &mut Progress::default(),
        )
        .unwrap();
        if source_changed {
            fs::write(second.path(), b"new version").unwrap();
        } else {
            fs::write(target.join("SECOND.txt").path(), b"another writer").unwrap();
        }
        assert!(plan.apply(&mut Progress::default()).is_err());
        assert_eq!(fs::read(first.path()).unwrap(), b"first");
        assert!(second.path().exists() && !target.join("first.txt").path().exists());
    }
}
