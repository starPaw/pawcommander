use super::*;

#[test]
fn archive_edit_dialogs_run_the_queue_and_refresh_the_pane() {
    use crate::vfs::ArchiveEdit;
    use egui::accesskit::{Action as AxAction, ActionRequest, Role, TreeId};
    use std::fs;

    let root = tempfile::tempdir().unwrap();
    let tar_path = root.path().join("fixtures.tar");
    let mut archive = tar::Builder::new(fs::File::create(&tar_path).unwrap());
    let mut header = tar::Header::new_gnu();
    header.set_size(4);
    archive
        .append_data(&mut header, "keep.txt", &b"keep"[..])
        .unwrap();
    drop(archive.into_inner().unwrap());
    let peer = root.path().join("peer");
    fs::create_dir(&peer).unwrap();
    let addition = Location::Local(peer.join("new.txt"));
    fs::write(addition.path(), b"disposable additions").unwrap();
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.history = History::for_test(root.path().join("history.json"), &ctx);
    app.settings.split = true;
    app.panes[1].tab_mut().navigate(peer.into(), &ctx);
    wait_for_directory(app.panes[1].tab_mut());
    app.panes[1].tab_mut().select_revealed(addition.clone());
    let file = Location::Local(tar_path.clone());
    let root_member = file.browse_archive().unwrap();
    app.action(Action::Open(0, file), &ctx);
    wait_for_directory(app.tab_mut());
    assert_eq!(app.tab().path, root_member);

    let draw = |app: &mut PawCommander, events| {
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(980.0, 720.0),
                )),
                events,
                ..Default::default()
            },
            |ui| app.dialogs(ui.ctx()),
        )
    };
    let click = |app: &mut PawCommander, label: &'static str| {
        draw(app, vec![]);
        let output = draw(app, vec![]).platform_output.accesskit_update.unwrap();
        let button = output
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == Role::Button
                    && node.label() == Some(crate::i18n::text(label))
                    && !node.is_disabled()
            })
            .unwrap_or_else(|| panic!("expected visible button {label}"))
            .0;
        draw(
            app,
            vec![egui::Event::AccessKitActionRequest(ActionRequest {
                action: AxAction::Click,
                target_tree: TreeId::ROOT,
                target_node: button,
                data: None,
            })],
        );
        assert!(app.dialog.is_none());
    };
    let finish = |app: &mut PawCommander| {
        poll_queue_until(app, &ctx, |app| app.job.is_none());
        wait_for_directory(app.tab_mut());
        assert!(app.notice.as_ref().is_some_and(|(_, error)| !error));
    };
    app.run_shortcut(Command::AddToArchive, &ctx);
    assert!(matches!(&app.dialog, Some(Dialog::ArchiveEdit {
        edit: ArchiveEdit::Add { sources, .. }, ..
    }) if sources == std::slice::from_ref(&addition)));
    click(&mut app, "Добавить");
    finish(&mut app);
    let added = root_member.join("new.txt");
    assert!(app.tab().entries.iter().any(|entry| entry.path == added));

    app.tab_mut().select_revealed(added);
    app.run_shortcut(Command::Rename, &ctx);
    let Some(Dialog::ArchiveEdit {
        edit: ArchiveEdit::Rename { name, .. },
        ..
    }) = &mut app.dialog
    else {
        panic!("expected the archive rename dialog");
    };
    *name = "renamed.txt".into();
    click(&mut app, "Переименовать");
    finish(&mut app);
    let renamed = root_member.join("renamed.txt");
    assert!(app.tab().entries.iter().any(|entry| entry.path == renamed));

    app.tab_mut().select_revealed(renamed);
    let before = fs::read(&tar_path).unwrap();
    app.run_shortcut(Command::RemoveFromArchive, &ctx);
    click(&mut app, "Отмена");
    assert_eq!(fs::read(&tar_path).unwrap(), before);
    app.run_shortcut(Command::RemoveFromArchive, &ctx);
    click(&mut app, "Удалить");
    finish(&mut app);
    assert_eq!(app.tab().entries.len(), 1);
    assert_eq!(app.tab().entries[0].name, "keep.txt");
    assert_eq!(fs::read(addition.path()).unwrap(), b"disposable additions");
}

#[test]
fn archive_open_navigation_session_refresh_and_search_use_the_pane() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("fixture.tar");
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o644);
    header.set_size(4);
    archive
        .append_data(&mut header, "outer/inner/note.txt", &b"note"[..])
        .unwrap();
    let bytes = archive.into_inner().unwrap();
    std::fs::write(&path, &bytes).unwrap();
    std::fs::create_dir(root.path().join("directory.zip")).unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let local = Location::Local(root.path().into());
    let file = Location::Local(path.clone());
    let archive = file.browse_archive().unwrap();
    wait_for_directory(app.tab_mut());
    app.tab_mut().select_revealed(file.clone());
    app.run_shortcut(Command::Open, &ctx);
    wait_for_directory(app.tab_mut());
    assert_eq!(app.tab().path, archive);
    let outer = archive.join("outer");
    app.action(Action::Open(0, outer.clone()), &ctx);
    wait_for_directory(app.tab_mut());
    let inner = outer.join("inner");
    assert_eq!(app.tab().entries[0].path, inner);
    app.action(Action::Open(0, inner.clone()), &ctx);
    wait_for_directory(app.tab_mut());
    let note = inner.join("note.txt");
    assert_eq!(app.tab().entries[0].path, note);
    app.tab_mut().select_revealed(note.clone());
    app.action(Action::Trash, &ctx);
    app.action(Action::Open(0, note), &ctx);
    assert!(app.dialog.is_none());
    assert!(app.notice.as_ref().is_some_and(|(_, error)| *error));
    assert_eq!(app.tab().path, inner);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(!root.path().join("outer").exists());

    let mut restored = Tab::from_session(app.tab().session(), root.path(), &ctx);
    wait_for_directory(&mut restored);
    assert_eq!(restored.path, inner);
    assert_eq!(restored.entries[0].name, "note.txt");

    app.action(Action::Back, &ctx);
    wait_for_directory(app.tab_mut());
    assert_eq!(app.tab().path, outer);
    app.action(Action::Forward, &ctx);
    wait_for_directory(app.tab_mut());
    assert_eq!(app.tab().path, inner);
    std::fs::write(&path, b"corrupt").unwrap();
    app.action(Action::Refresh, &ctx);
    wait_for_directory(app.tab_mut());
    assert!(app.tab().error.is_some());
    assert!(app.tab().entries.is_empty());
    std::fs::write(&path, &bytes).unwrap();
    for _ in 0..3 {
        app.action(Action::Up, &ctx);
    }
    wait_for_directory(app.tab_mut());
    assert_eq!(app.tab().path, local);
    let target = TabTarget {
        pane: 0,
        tab: app.tab().id,
        location: local.clone(),
    };
    app.action(Action::Open(0, local.join("directory.zip")), &ctx);
    wait_for_directory(app.tab_mut());
    assert!(app.tab().path.local().is_some());
    app.dispatch_search_result(
        ReadyAction::Open {
            path: path.clone().into(),
            directory: false,
            target: target.clone(),
        },
        &ctx,
    );
    assert_eq!(app.tab().path, local.join("directory.zip"));
    app.action(Action::Up, &ctx);
    wait_for_directory(app.tab_mut());
    app.dispatch_search_result(
        ReadyAction::Open {
            path: path.into(),
            directory: false,
            target,
        },
        &ctx,
    );
    wait_for_directory(app.tab_mut());
    assert_eq!(app.tab().path, archive);
    assert_eq!(app.tab().entries[0].path, outer);
    app.tab_mut().browse_columns = true;
    app.action(Action::Back, &ctx);
    assert_eq!(app.tab().path, local);
    assert!(app.tab().browse_columns);
    app.action(Action::Forward, &ctx);
    assert_eq!(app.tab().path, archive);
    assert!(app.tab().browse_columns);
}

#[test]
fn progressive_directory_shows_first_entry_before_stall_and_keeps_partial_results() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("first"), b"first").unwrap();
    std::fs::write(root.path().join("second"), b"second").unwrap();
    let ctx = egui::Context::default();
    let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
    let mut entries = fs_ops::read_directory(&tab.path, &|| false)
        .unwrap()
        .into_iter();
    let (tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Progressive);
    tab.loading = Some(load);
    let (first_sent, first_received) = mpsc::channel();
    let (resume, remainder) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        send_progressive_listing(
            |visit| {
                visit(entries.next().unwrap());
                first_sent.send(()).unwrap();
                remainder.recv().unwrap();
                visit(entries.next().unwrap());
                Err("fixture read failed".into())
            },
            |update| tx.send(update).is_ok(),
        );
    });
    first_received.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(tab.poll());
    assert!(tab.blocking_load());
    assert_eq!(tab.entries.len(), 1);
    assert_eq!(tab.entries[0].name, "first");
    assert_eq!(tab.entries[0].size, 5);
    let first = tab.entries[0].path.clone();
    tab.selected.insert(first.clone());
    resume.send(()).unwrap();
    wait_for_directory(&mut tab);
    worker.join().unwrap();
    assert_eq!(tab.entries.len(), 2);
    assert_eq!(tab.selected, BTreeSet::from([first]));
    let error = tab.error.as_deref().unwrap();
    assert!(error.contains("incomplete folder listing"));
    assert!(error.contains("fixture read failed"));

    tab.auto_refresh(&ctx, tab.next_auto_refresh);
    wait_for_directory(&mut tab);
    assert!(tab.has_complete_listing);
    assert!(tab.error.is_none());
}

#[test]
fn navigating_cancels_progressive_directory_and_discards_queued_entries() {
    let old = tempfile::tempdir().unwrap();
    let new = tempfile::tempdir().unwrap();
    std::fs::write(old.path().join("old-first"), b"old").unwrap();
    std::fs::write(old.path().join("old-second"), b"old").unwrap();
    std::fs::write(new.path().join("current"), b"new").unwrap();
    let ctx = egui::Context::default();
    let mut tab = Tab::new(old.path().to_path_buf().into(), &ctx);
    let (tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Progressive);
    let cancelled = load.cancelled.clone();
    tab.loading = Some(load);
    let location = tab.path.clone();
    let (first_sent, first_received) = mpsc::channel();
    let (resume, remainder) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut visited = 0;
        let cancelled = || cancelled.load(Ordering::Relaxed);
        send_progressive_listing(
            |visit| {
                fs_ops::visit_directory(&location, &cancelled, |entry| {
                    visited += 1;
                    visit(entry);
                    if visited == 1 {
                        first_sent.send(()).unwrap();
                        remainder.recv().unwrap();
                    }
                })
            },
            |update| !cancelled() && tx.send(update).is_ok(),
        );
        visited
    });
    first_received.recv_timeout(Duration::from_secs(5)).unwrap();
    tab.navigate(new.path().to_path_buf().into(), &ctx);
    resume.send(()).unwrap();
    wait_for_directory(&mut tab);
    assert_eq!(worker.join().unwrap(), 1);
    assert_eq!(tab.entries.len(), 1);
    assert_eq!(tab.entries[0].name, "current");
    assert!(tab.error.is_none());
}

#[test]
fn automatic_refresh_reads_real_files_preserves_state_and_recovers_after_error() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("watched");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("keep.txt"), "old").unwrap();
    std::fs::write(folder.join("rename.txt"), "rename").unwrap();
    let ctx = egui::Context::default();
    let mut tab = Tab::new(folder.clone().into(), &ctx);
    wait_for_directory(&mut tab);
    let keep = tab.path.join("keep.txt");
    let renamed = tab.path.join("rename.txt");
    tab.selected = BTreeSet::from([keep.clone(), renamed.clone()]);
    tab.cursor = Some(keep.clone());
    tab.anchor = Some(renamed);
    tab.filter = ".txt".into();
    tab.sort = Sort::Size;
    tab.descending = true;
    // Changes are made independently of PawCommander's operation/refresh path.
    std::fs::write(folder.join("keep.txt"), "changed file contents").unwrap();
    std::fs::rename(folder.join("rename.txt"), folder.join("renamed.txt")).unwrap();
    std::fs::write(folder.join("new.txt"), "new").unwrap();
    tab.auto_refresh(&ctx, tab.next_auto_refresh - Duration::from_millis(1));
    assert!(tab.loading.is_none(), "poll must respect its interval");
    tab.auto_refresh(&ctx, tab.next_auto_refresh);
    assert!(tab.automatic_loading());
    assert!(
        !tab.blocking_load(),
        "automatic reads must not add a spinner"
    );
    wait_for_directory(&mut tab);
    assert_eq!(tab.selected, BTreeSet::from([keep.clone()]));
    assert_eq!(tab.cursor, Some(keep.clone()));
    assert!(tab.anchor.is_none());
    assert!(tab.scroll_to.is_none());
    assert_eq!(tab.filter, ".txt");
    assert!(tab.sort == Sort::Size && tab.descending);
    assert_eq!(tab.entries.len(), 3);
    assert_eq!(
        tab.entries.iter().find(|e| e.path == keep).unwrap().size,
        21
    );
    assert!(tab.entries.iter().any(|e| e.name == "renamed.txt"));

    // A missing/remounted volume keeps the last readable listing and retries.
    let moved = root.path().join("offline");
    std::fs::rename(&folder, &moved).unwrap();
    tab.auto_refresh(&ctx, tab.next_auto_refresh);
    wait_for_directory(&mut tab);
    assert!(tab
        .error
        .as_ref()
        .unwrap()
        .contains("last successful listing"));
    assert_eq!(tab.entries.len(), 3);
    assert!(tab.selected.contains(&keep));
    std::fs::rename(&moved, &folder).unwrap();
    std::fs::remove_file(folder.join("keep.txt")).unwrap();
    tab.auto_refresh(&ctx, tab.next_auto_refresh);
    wait_for_directory(&mut tab);
    assert!(tab.error.is_none());
    assert_eq!(tab.entries.len(), 2);
    assert!(tab.selected.is_empty());
    assert!(tab.cursor.is_none());
}

#[test]
fn automatic_refresh_is_single_flight_and_ignores_stale_reads() {
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("current"), "current").unwrap();
    let ctx = egui::Context::default();
    let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
    wait_for_directory(&mut tab);
    let (stale_tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Automatic);
    let cancelled = load.cancelled.clone();
    tab.loading = Some(load);
    tab.auto_refresh(&ctx, tab.next_auto_refresh);
    assert!(tab.automatic_loading());
    // Sending to the original receiver proves no second scan replaced it.
    stale_tx
        .send(DirectoryRead::Finished(Ok(Some(vec![]))))
        .unwrap();
    tab.navigate(other.path().to_path_buf().into(), &ctx);
    assert!(cancelled.load(Ordering::Relaxed));
    wait_for_directory(&mut tab);
    assert_eq!(tab.entries.len(), 1);
    assert_eq!(tab.entries[0].name, "current");
}

#[test]
fn automatic_refresh_schedules_remote_tab_without_reconnecting_or_clearing_it() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.settings.split = false;
    let tab = app.tab_mut();
    wait_for_directory(tab);
    tab.path = Location::Sftp {
        endpoint: Endpoint {
            host: "127.0.0.1".into(),
            port: 0,
            user: "test".into(),
            key: None,
        },
        path: "/".into(),
    };
    let keep = tab.path.join("keep.txt");
    tab.entries.replace(vec![FileEntry {
        path: keep.clone(),
        name: "keep.txt".into(),
        is_dir: false,
        is_symlink: false,
        size: 42,
        modified: None,
        metadata: Default::default(),
    }]);
    tab.selected.insert(keep.clone());
    tab.next_auto_refresh = Instant::now();
    app.poll(&ctx);
    let tab = app.tab_mut();
    assert!(tab.automatic_loading() && !tab.blocking_load());
    let started = Instant::now();
    wait_for_directory(tab);
    assert_eq!(tab.entries.len(), 1);
    assert_eq!(tab.selected, BTreeSet::from([keep]));
    assert!(tab.error.is_none(), "poll should skip an uncached session");
    assert!(tab.next_auto_refresh >= started + SFTP_REFRESH_INTERVAL);
}

#[test]
fn automatic_refresh_schedules_only_displayed_tabs_and_waits_for_pointer_release() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.settings.split = false;
    let background = Tab::new(root.path().to_path_buf().into(), &ctx);
    app.panes[0].tabs.push(background);
    for pane in &mut app.panes {
        for tab in &mut pane.tabs {
            wait_for_directory(tab);
            tab.next_auto_refresh = Instant::now() - AUTO_REFRESH_INTERVAL;
        }
    }
    let pointer = egui::Event::PointerButton {
        pos: egui::pos2(100.0, 100.0),
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Default::default(),
    };
    let _ = ctx.run_test_ui(
        egui::RawInput {
            events: vec![pointer],
            ..Default::default()
        },
        |ui| app.poll(ui.ctx()),
    );
    assert!(
        app.panes[0].tab().loading.is_none(),
        "held pointer postpones automatic read"
    );
    let _ = ctx.run_test_ui(
        egui::RawInput {
            events: vec![egui::Event::PointerButton {
                pos: egui::pos2(100.0, 100.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            }],
            ..Default::default()
        },
        |_| {},
    );
    // Start in a fresh input frame with no held/released pointer.
    let _ = ctx.run_test_ui(egui::RawInput::default(), |ui| app.poll(ui.ctx()));
    assert!(app.panes[0].tabs[0].automatic_loading());
    assert!(
        app.panes[0].tabs[1].loading.is_none(),
        "inactive tab stays idle"
    );
    assert!(
        app.panes[1].tab().loading.is_none(),
        "hidden pane stays idle"
    );
    wait_for_directory(app.panes[0].tab_mut());
    app.settings.split = true;
    app.poll(&ctx);
    assert!(
        app.panes[1].tab().automatic_loading(),
        "displayed pane resumes scanning"
    );
    wait_for_directory(app.panes[1].tab_mut());
}

#[test]
fn automatic_refresh_and_session_restore_preserve_scroll_in_list_and_grid() {
    fn draw(
        ctx: &egui::Context,
        pane: &mut Pane,
        settings: &Settings,
        index: usize,
    ) -> Vec<(String, egui::Pos2)> {
        let output = ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(640.0, 550.0),
                )),
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    render_pane(
                        ui,
                        pane,
                        PaneOptions {
                            index,
                            active: true,
                            settings,
                            folder_views: &FolderViews::new(),
                            idle: true,
                            zed_available: false,
                        },
                        &mut vec![],
                        &mut DropTargets::default(),
                        &mut TabDropTargets::default(),
                        None,
                    );
                });
            },
        );
        output
            .shapes
            .into_iter()
            .filter_map(|clipped| match clipped.shape {
                egui::Shape::Text(text) if text.galley.text().starts_with("file") => {
                    Some((text.galley.text().to_owned(), text.pos))
                }
                _ => None,
            })
            .collect()
    }
    for grid in [false, true] {
        let root = tempfile::tempdir().unwrap();
        for i in 0..100 {
            std::fs::write(root.path().join(format!("file{i:03}")), "content").unwrap();
        }
        let ctx = egui::Context::default();
        let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
        wait_for_directory(&mut tab);
        let selected = tab.path.join("file035");
        tab.selected.insert(selected.clone());
        let mut pane = Pane::new(vec![tab]);
        let settings = Settings {
            grid,
            ..Default::default()
        };
        draw(&ctx, &mut pane, &settings, 0);
        pane.tab_mut().scroll_to = Some(selected.clone());
        draw(&ctx, &mut pane, &settings, 0);
        let before = draw(&ctx, &mut pane, &settings, 0);
        assert!(
            before.iter().any(|(name, _)| name == "file035"),
            "scrolled middle row is rendered"
        );
        assert!(
            !before.iter().any(|(name, _)| name == "file000"),
            "test must start below the top"
        );
        std::fs::write(root.path().join("file999"), "external").unwrap();
        pane.tab_mut()
            .auto_refresh(&ctx, Instant::now() + AUTO_REFRESH_INTERVAL);
        assert_eq!(
            draw(&ctx, &mut pane, &settings, 0),
            before,
            "no layout shift while reading"
        );
        wait_for_directory(pane.tab_mut());
        assert_eq!(
            draw(&ctx, &mut pane, &settings, 0),
            before,
            "refresh retains pixel offset and rows"
        );
        assert!(pane.tab().selected.contains(&selected));
        assert!(pane.tab().scroll_to.is_none());

        let saved = pane.session();
        let offset = saved.tabs[0].scroll_offset;
        assert!(offset > 0.0);
        let reopened_ctx = egui::Context::default();
        let mut reopened = Pane::from_session(saved, root.path(), &reopened_ctx);
        let (_tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Progressive);
        reopened.tab_mut().loading = Some(load);
        reopened
            .tab_mut()
            .entries
            .extend(vec![pane.tab().entries[0].clone()]);
        draw(&reopened_ctx, &mut reopened, &settings, 0);
        assert_eq!(reopened.tab().scroll_restore, Some(offset));
        assert_eq!(reopened.session().tabs[0].scroll_offset, offset);

        reopened
            .tab_mut()
            .finish_load(Ok(Some(pane.tab().entries.to_vec())));
        draw(&reopened_ctx, &mut reopened, &settings, 0);
        assert_eq!(draw(&reopened_ctx, &mut reopened, &settings, 0), before);
        assert_eq!(reopened.tab().scroll_restore, None);
        assert_eq!(reopened.tab().scroll_offset, offset);
        let previous_scope = reopened.tab().file_scroll_scope;
        draw(&reopened_ctx, &mut reopened, &settings, 1);
        assert_ne!(reopened.tab().file_scroll_scope, previous_scope);
        assert_eq!(reopened.tab().scroll_offset, offset);
    }
}
