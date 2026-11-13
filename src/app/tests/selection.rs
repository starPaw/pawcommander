use super::*;

#[test]
fn selection_rules_use_only_visible_current_folder_rows() {
    let root = tempfile::tempdir().unwrap();
    for name in [
        "report.TXT",
        "report-2.txt",
        "report.csv",
        "other.txt",
        ".report.txt",
    ] {
        std::fs::write(root.path().join(name), b"fixture").unwrap();
    }
    std::fs::create_dir(root.path().join("report-folder.txt")).unwrap();
    let location = Location::from(root.path().to_path_buf());
    let ctx = egui::Context::default();
    let mut tab = Tab::new(location.clone(), &ctx);
    wait_for_directory(&mut tab);
    tab.filter = "report".into();
    let view = FolderView {
        mode: FolderMode::Columns,
        ..FolderView::default()
    };
    let txt = location.join("report.TXT");
    let txt2 = location.join("report-2.txt");
    let csv = location.join("report.csv");
    let folder = location.join("report-folder.txt");
    tab.selected = BTreeSet::from([csv.clone(), location.join("other.txt")]);
    let extension = RuleKind::Extension.compile(".txt").unwrap();
    let mut scan = SelectionScan::new(extension, tab.visible(view));
    assert!(!scan.step(&tab.entries, Instant::now()));
    assert!(scan.matches.is_empty());
    assert!(scan.step(&tab.entries, Instant::now() + Duration::from_secs(1)));
    tab.change_selection(view, SelectionChange::Matches(scan.matches));
    assert_eq!(tab.selected, BTreeSet::from([txt.clone(), txt2.clone()]));
    tab.change_selection(view, SelectionChange::Restore);
    assert_eq!(tab.selected, BTreeSet::from([csv.clone()]));

    let mask = RuleKind::Mask.compile("REPORT*.TXT").unwrap();
    let mut scan = SelectionScan::new(mask, tab.visible(view));
    assert!(scan.step(&tab.entries, Instant::now() + Duration::from_secs(1)));
    tab.change_selection(view, SelectionChange::Matches(scan.matches));
    assert_eq!(tab.selected, BTreeSet::from([txt, txt2, folder]));
    tab.change_selection(view, SelectionChange::Invert);
    assert_eq!(tab.selected, BTreeSet::from([csv]));
    tab.filter = "report-2".into();
    tab.change_selection(view, SelectionChange::Restore);
    assert_eq!(
        tab.selected,
        BTreeSet::from([location.join("report-2.txt")])
    );
    assert!(tab.previous_selection.is_none());
}

#[test]
fn selection_scan_discards_matches_if_the_visible_rows_change() {
    let root = tempfile::tempdir().unwrap();
    for name in ["first.txt", "second.txt"] {
        std::fs::write(root.path().join(name), b"fixture").unwrap();
    }
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    wait_for_directory(app.tab_mut());
    let first = app.tab().path.join("first.txt");
    let second = app.tab().path.join("second.txt");
    app.tab_mut().selected.insert(second.clone());
    app.run_shortcut(Command::SelectByMask, &ctx);
    let mut scan = SelectionScan::new(
        RuleKind::Mask.compile("*.txt").unwrap(),
        app.tab().visible(app.folder_view(app.active)),
    );
    // Model a completed slice whose matches have yet to be published.
    scan.next = 1;
    scan.matches.insert(first);
    if let Some(Dialog::SelectionRule { scan: pending, .. }) = &mut app.dialog {
        *pending = Some(scan);
    } else {
        panic!("expected a selection rule dialog");
    }
    app.tab_mut().filter = "second".into();
    let _ = ctx.run_test_ui(egui::RawInput::default(), |ui| app.dialogs(ui.ctx()));
    assert!(app.dialog.is_none());
    assert_eq!(app.selected(), vec![second]);
    assert!(app.tab().previous_selection.is_none());
}

#[test]
fn selection_restore_discards_disappeared_and_replaced_entries_and_is_tab_local() {
    let root = tempfile::tempdir().unwrap();
    let location = Location::from(root.path().to_path_buf());
    let stable = location.join("stable");
    let gone = location.join("gone");
    let replaced = location.join("replaced");
    for file in [&stable, &gone, &replaced] {
        std::fs::write(file.path(), b"fixture").unwrap();
    }
    let ctx = egui::Context::default();
    let mut tab = Tab::new(location.clone(), &ctx);
    wait_for_directory(&mut tab);
    let view = FolderView::default();
    tab.change_selection(view, SelectionChange::All);
    tab.change_selection(view, SelectionChange::Invert);
    let mut other = Tab::new(location.clone(), &ctx);
    wait_for_directory(&mut other);
    other.change_selection(view, SelectionChange::Restore);
    assert!(other.selected.is_empty());

    std::fs::remove_file(gone.path()).unwrap();
    std::fs::remove_file(replaced.path()).unwrap();
    std::fs::create_dir(replaced.path()).unwrap();
    tab.reload(&ctx);
    wait_for_directory(&mut tab);
    std::fs::write(gone.path(), b"fixture").unwrap();
    tab.reload(&ctx);
    wait_for_directory(&mut tab);
    tab.change_selection(view, SelectionChange::Restore);
    assert_eq!(tab.selected, BTreeSet::from([stable]));
    tab.change_selection(view, SelectionChange::Invert);
    tab.navigate(replaced, &ctx);
    tab.history_go(-1, &ctx);
    wait_for_directory(&mut tab);
    assert!(tab.previous_selection.is_none());
}

#[test]
fn selection_commands_retarget_file_focus_and_keep_select_all_cursor() {
    let root = tempfile::tempdir().unwrap();
    let location = Location::from(root.path().to_path_buf());
    let first = location.join("a.txt");
    let second = location.join("b.csv");
    for file in [&first, &second] {
        std::fs::write(file.path(), b"fixture").unwrap();
    }
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    wait_for_directory(app.tab_mut());
    let first_id = app.tab().entry_id(&first);
    let second_id = app.tab().entry_id(&second);
    app.tab_mut()
        .rendered_rows
        .extend([(first_id, first.clone()), (second_id, second.clone())]);
    app.tab_mut().selected = BTreeSet::from([first.clone()]);
    app.tab_mut().cursor = Some(first.clone());
    app.tab_mut().anchor = Some(second.clone());
    ctx.memory_mut(|memory| memory.request_focus(first_id));

    app.run_shortcut(Command::SelectAll, &ctx);
    assert_eq!(app.tab().anchor.as_ref(), Some(&second));
    assert_eq!(app.tab().cursor.as_ref(), Some(&first));
    assert!(app.tab().scroll_to.is_none());
    app.run_shortcut(Command::RestoreSelection, &ctx);
    assert_eq!(app.selected(), vec![first.clone()]);
    // A same-input-batch movement can give focus to a row before it renders.
    app.tab_mut().rendered_rows.remove(&first_id);
    app.run_shortcut(Command::InvertSelection, &ctx);
    assert_eq!(app.selected(), vec![second.clone()]);
    assert!(ctx.memory(|memory| memory.has_focus(second_id)));
    app.run_shortcut(Command::RestoreSelection, &ctx);
    assert_eq!(app.selected(), vec![first.clone()]);
    assert!(ctx.memory(|memory| memory.has_focus(first_id)));
    app.run_pane_action(Action::Shortcut(Command::Trash), &ctx);
    assert!(matches!(app.dialog, Some(Dialog::Trash { ref paths }) if paths == &[first]));
}

#[test]
fn filters_sort_and_range_selection() {
    let ctx = egui::Context::default();
    let root = tempfile::tempdir().unwrap();
    let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
    tab.entries.replace(
        [
            ("z.txt", false),
            ("B", true),
            ("a.txt", false),
            (".hidden", false),
        ]
        .into_iter()
        .map(|(name, is_dir)| FileEntry {
            path: root.path().join(name).into(),
            name: name.into(),
            is_dir,
            is_symlink: false,
            size: 1,
            modified: None,
            metadata: Default::default(),
        })
        .collect(),
    );
    let visible = visible_rows(&tab, false);
    assert_eq!(visible.as_slice(), &[1, 2, 0]);
    tab.choose(1, &visible, egui::Modifiers::NONE);
    tab.choose(0, &visible, egui::Modifiers::SHIFT);
    assert_eq!(tab.selected.len(), 3);
    tab.filter = "TXT".into();
    assert_eq!(visible_rows(&tab, false).as_slice(), &[2, 0]);
    tab.descending = true;
    tab.filter.clear();
    assert_eq!(visible_rows(&tab, false).as_slice(), &[1, 0, 2]);
}

#[test]
fn visible_rows_follow_batches_query_changes_and_replacements() {
    let ctx = egui::Context::default();
    let root = tempfile::tempdir().unwrap();
    let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
    let entry = |name: &str, size: u64, is_dir: bool| FileEntry {
        path: root.path().join(name).into(),
        name: name.into(),
        is_dir,
        is_symlink: false,
        size,
        modified: Some(std::time::UNIX_EPOCH + Duration::from_secs(size)),
        metadata: Default::default(),
    };
    let (tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Progressive);
    tab.loading = Some(load);
    tx.send(DirectoryRead::Batch(vec![
        entry("z.txt", 20, false),
        entry("Ä.txt", 4, false),
        entry(".hidden", 8, false),
    ]))
    .unwrap();
    assert!(tab.poll());
    let initial = visible_rows(&tab, false);
    assert_eq!(initial.as_slice(), &[0, 1]);
    assert!(Rc::ptr_eq(&initial, &visible_rows(&tab, false)));
    tab.descending = true;
    assert_eq!(visible_rows(&tab, false).as_slice(), &[1, 0]);

    tx.send(DirectoryRead::Batch(vec![
        entry("D", 2, true),
        entry("ä.txt", 4, false),
    ]))
    .unwrap();
    assert!(tab.poll());
    assert_eq!(visible_rows(&tab, false).as_slice(), &[3, 4, 1, 0]);
    assert_eq!(initial.as_slice(), &[0, 1]);
    tab.filter = "Ä.TXT".into();
    assert_eq!(visible_rows(&tab, false).as_slice(), &[4, 1]);
    tab.filter.clear();
    assert_eq!(visible_rows(&tab, true).as_slice(), &[3, 4, 1, 0, 2]);
    tab.sort = Sort::Modified;
    assert_eq!(visible_rows(&tab, true).as_slice(), &[3, 0, 2, 4, 1]);

    tx.send(DirectoryRead::Finished(Ok(None))).unwrap();
    assert!(!tab.poll());
    let (tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Reload);
    tab.loading = Some(load);
    let mut replacement = tab.entries.to_vec();
    replacement[0] = entry("B.txt", 1, false);
    tx.send(DirectoryRead::Finished(Ok(Some(replacement))))
        .unwrap();
    assert!(!tab.poll());
    assert_eq!(visible_rows(&tab, true).as_slice(), &[3, 2, 4, 1, 0]);
    tab.sort = Sort::Name;
    tab.filter = "B.TXT".into();
    assert_eq!(visible_rows(&tab, false).as_slice(), &[0]);

    let next = tempfile::tempdir().unwrap();
    std::fs::write(next.path().join("next"), "").unwrap();
    tab.navigate(next.path().to_path_buf().into(), &ctx);
    assert!(visible_rows(&tab, false).is_empty());
    wait_for_directory(&mut tab);
    assert_eq!(visible_rows(&tab, false).as_slice(), &[0]);
    assert_eq!(tab.entries[0].name, "next");
}
