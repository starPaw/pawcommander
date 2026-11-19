use super::*;

#[test]
fn search_result_is_selected_after_loading_and_stale_result_is_reported() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join(".hidden файл");
    std::fs::write(&path, "test").unwrap();
    let location: Location = path.clone().into();
    let ctx = egui::Context::default();
    let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
    let (tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Reload);
    tab.loading = Some(load);
    tab.reveal_after_load = Some(location.clone());
    tx.send(DirectoryRead::Finished(
        fs_ops::read_directory(&tab.path, &|| false).map(Some),
    ))
    .unwrap();
    tab.poll();
    assert_eq!(tab.selected, BTreeSet::from([location.clone()]));
    assert_eq!(tab.cursor, Some(location.clone()));
    assert_eq!(tab.scroll_to, Some(location.clone()));
    assert!(tab.reveal_after_load.is_none());
    assert!(visible_rows(&tab, true)
        .iter()
        .any(|i| tab.entries[*i].path == location));

    std::fs::remove_file(path).unwrap();
    let (tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Reload);
    tab.loading = Some(load);
    tab.reveal_after_load = Some(location);
    tx.send(DirectoryRead::Finished(
        fs_ops::read_directory(&tab.path, &|| false).map(Some),
    ))
    .unwrap();
    tab.poll();
    assert!(tab.selected.is_empty());
    assert!(tab
        .error
        .unwrap()
        .contains("search result no longer exists"));
}

#[test]
fn search_results_copy_to_captured_location_and_do_not_reveal_in_a_changed_tab() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let destination = root.path().join("destination");
    let other = root.path().join("other");
    for folder in [&source_dir, &destination, &other] {
        std::fs::create_dir(folder).unwrap();
    }
    let path = source_dir.join("found.txt");
    std::fs::write(&path, "search hit").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(source_dir.clone()));
    app.settings.split = true;
    let target = TabTarget {
        pane: app.active,
        tab: app.tab().id,
        location: app.tab().path.clone(),
    };
    app.tab_mut().navigate(other.clone().into(), &ctx);
    app.panes[1 - app.active]
        .tab_mut()
        .navigate(other.clone().into(), &ctx);
    app.dispatch_search_result(
        ReadyAction::Reveal {
            path: path.clone().into(),
            target,
        },
        &ctx,
    );
    assert_eq!(app.tab().path.local(), Some(other.as_path()));
    assert!(app.tab().reveal_after_load.is_none());

    app.dispatch_search_result(
        ReadyAction::Copy {
            paths: vec![path.clone().into()],
            destination: destination.clone().into(),
        },
        &ctx,
    );
    assert!(app.job.is_some());
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.job.is_some() && Instant::now() < deadline {
        app.poll(&ctx);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(app.job.is_none());
    assert_eq!(
        std::fs::read(destination.join("found.txt")).unwrap(),
        b"search hit"
    );
    assert_eq!(std::fs::read(path).unwrap(), b"search hit");
    assert!(!other.join("found.txt").exists());
}
