use super::*;

#[test]
fn pointer_drag_on_tabs_reorders_transfers_and_cancels() {
    fn frame(
        ctx: &egui::Context,
        panes: &mut [Pane; 2],
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(940.0, 200.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                let ctx = &ui.ctx().clone();
                let mut file_targets = DropTargets::default();
                let mut tab_targets = TabDropTargets::default();
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.columns(2, |cols| {
                        for (i, ui) in cols.iter_mut().enumerate() {
                            tab_strip(
                                ui,
                                &mut panes[i],
                                i,
                                &mut vec![],
                                &mut file_targets,
                                &mut tab_targets,
                                true,
                            );
                        }
                    });
                });
                if let Some(drop) = tab_targets.finish(ctx, true) {
                    move_tab(panes, drop, ctx);
                }
            },
        )
    }

    fn label(output: &egui::FullOutput, name: &str) -> egui::Pos2 {
        output
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) if text.galley.text() == name => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .unwrap()
    }

    fn button(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }
    let ctx = egui::Context::default();
    let dir = tempfile::tempdir().unwrap();
    let make_tab = |name| Tab::new(dir.path().join(name).into(), &ctx);
    let mut panes = [
        Pane::new(vec![make_tab("alpha"), make_tab("beta")]),
        Pane::new(vec![make_tab("gamma")]),
    ];
    let alpha_id = panes[0].tab().id;
    let output = frame(&ctx, &mut panes, vec![]);
    let alpha = label(&output, "alpha");
    let after_beta = label(&output, "beta") + egui::vec2(43.0, 0.0);
    frame(
        &ctx,
        &mut panes,
        vec![egui::Event::PointerMoved(alpha), button(alpha, true)],
    );
    frame(
        &ctx,
        &mut panes,
        vec![egui::Event::PointerMoved(after_beta)],
    );
    let output = frame(&ctx, &mut panes, vec![button(after_beta, false)]);
    assert_eq!(panes[0].tabs[1].id, alpha_id);

    let alpha = label(&output, "alpha");
    let gamma = label(&output, "gamma");
    let before_gamma = gamma - egui::vec2(24.0, 0.0);
    frame(
        &ctx,
        &mut panes,
        vec![egui::Event::PointerMoved(alpha), button(alpha, true)],
    );
    frame(
        &ctx,
        &mut panes,
        vec![egui::Event::PointerMoved(before_gamma)],
    );
    let output = frame(&ctx, &mut panes, vec![button(before_gamma, false)]);
    assert_eq!(panes[0].tabs.len(), 1);
    assert_eq!(panes[1].tabs[0].id, alpha_id);

    let alpha = label(&output, "alpha");
    let beta = label(&output, "beta");
    frame(
        &ctx,
        &mut panes,
        vec![egui::Event::PointerMoved(alpha), button(alpha, true)],
    );
    frame(&ctx, &mut panes, vec![egui::Event::PointerMoved(beta)]);
    frame(
        &ctx,
        &mut panes,
        vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }],
    );
    frame(&ctx, &mut panes, vec![button(beta, false)]);
    assert_eq!(panes[0].tabs.len(), 1);
    assert_eq!(panes[1].tabs[0].id, alpha_id);
}

#[test]
fn tab_moves_keep_the_tab_state_and_leave_both_panes_usable() {
    let ctx = egui::Context::default();
    let root = tempfile::tempdir().unwrap();
    let path: Location = root.path().to_path_buf().into();
    let mut moved = Tab::new(path.clone(), &ctx);
    let id = moved.id;
    moved.filter = "report".into();
    moved.selected.insert(path.join("report.txt"));
    moved.history.insert(0, path.join("previous"));
    moved.history_index = 1;
    let other = Tab::new(path.clone(), &ctx);
    let other_id = other.id;
    let mut panes = [
        Pane::new(vec![moved, other]),
        Pane::new(vec![Tab::new(path.clone(), &ctx)]),
    ];
    panes[0].select_tab(1);
    assert!(move_tab(
        &mut panes,
        TabDrop {
            source: 0,
            id,
            destination: 0,
            slot: 2
        },
        &ctx
    ));
    assert_eq!(panes[0].tabs[0].id, other_id);
    assert_eq!(panes[0].tab().id, id);
    assert!(!move_tab(
        &mut panes,
        TabDrop {
            source: 0,
            id,
            destination: 0,
            slot: 1
        },
        &ctx
    ));

    assert!(move_tab(
        &mut panes,
        TabDrop {
            source: 0,
            id,
            destination: 1,
            slot: 0
        },
        &ctx
    ));
    assert_eq!(panes[0].tab().id, other_id);
    assert_eq!(panes[1].tab().id, id);
    assert_eq!(panes[1].tab().filter, "report");
    assert!(panes[1].tab().selected.contains(&path.join("report.txt")));
    assert_eq!(panes[1].tab().history[0], path.join("previous"));
    assert_eq!(panes[1].tab().history_index, 1);
    assert!(move_tab(
        &mut panes,
        TabDrop {
            source: 0,
            id: other_id,
            destination: 1,
            slot: 2
        },
        &ctx
    ));
    assert_eq!(panes[0].tabs.len(), 1);
    assert_ne!(panes[0].tab().id, other_id);
    assert_eq!(panes[1].tab().id, other_id);
    assert_eq!(panes[1].tabs.len(), 3);
}

#[test]
fn tab_moves_respect_pin_group_boundaries_in_both_panes() {
    let ctx = egui::Context::default();
    let root = tempfile::tempdir().unwrap();
    let make_tab = |name| Tab::new(root.path().join(name).into(), &ctx);
    let mut panes = [
        Pane::new(vec![make_tab("a"), make_tab("b"), make_tab("c")]),
        Pane::new(vec![make_tab("d"), make_tab("e")]),
    ];
    for (pane, index) in [(0, 0), (0, 1), (1, 0)] {
        panes[pane].toggle_pin(index);
    }
    let a = panes[0].tabs[0].id;
    let c = panes[0].tabs[2].id;
    assert!(!move_tab(
        &mut panes,
        TabDrop {
            source: 0,
            id: c,
            destination: 0,
            slot: 0,
        },
        &ctx,
    ));
    assert!(move_tab(
        &mut panes,
        TabDrop {
            source: 0,
            id: a,
            destination: 0,
            slot: 3,
        },
        &ctx,
    ));
    assert_eq!(panes[0].tabs[1].id, a);
    assert_eq!(panes[0].pinned_count(), 2);

    for (id, slot) in [(a, 2), (c, 0)] {
        assert!(move_tab(
            &mut panes,
            TabDrop {
                source: 0,
                id,
                destination: 1,
                slot,
            },
            &ctx,
        ));
    }
    assert_eq!(panes[0].pinned_count(), 1);
    assert_eq!(panes[1].pinned_count(), 2);
    assert_eq!(panes[1].tabs[1].id, a);
    assert_eq!(panes[1].tabs[2].id, c);
    assert!(panes.iter().all(|pane| pane.closed_tabs.is_empty()));
}

#[test]
fn tab_context_menu_blocks_pane_shortcuts_until_escape_dismisses_it() {
    use egui::{Event, Key, Modifiers, PointerButton};

    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("tab-menu");
    std::fs::create_dir(&folder).unwrap();
    let file = folder.join("selected.txt");
    std::fs::write(&file, "example").unwrap();
    let file: Location = file.into();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut frame = eframe::Frame::_new_kittest();
    let mut app = PawCommander::new(&cc, Some(folder));
    app.action(Action::NewTab(0), &ctx);
    wait_for_directory(app.tab_mut());
    app.tab_mut().selected.insert(file.clone());

    let mut run = |app: &mut PawCommander, events| {
        let mut input = egui::RawInput {
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(1440.0, 860.0),
            )),
            ..Default::default()
        };
        eframe::App::raw_input_hook(app, &ctx, &mut input);
        ctx.run_test_ui(input, |ui| {
            eframe::App::logic(app, ui.ctx(), &mut frame);
            eframe::App::ui(app, ui, &mut frame);
        })
    };
    let key = |key, modifiers| {
        [true, false].map(|pressed| Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers,
        })
    };
    let output = run(&mut app, vec![]);
    let pos = output
        .shapes
        .iter()
        .find_map(|clipped| match &clipped.shape {
            egui::Shape::Text(text) if text.galley.text() == "tab-menu" => {
                Some(text.pos + text.galley.size() * 0.5)
            }
            _ => None,
        })
        .unwrap();
    for pressed in [true, false] {
        run(
            &mut app,
            vec![
                Event::PointerMoved(pos),
                Event::PointerButton {
                    pos,
                    button: PointerButton::Secondary,
                    pressed,
                    modifiers: Modifiers::NONE,
                },
            ],
        );
    }
    assert!(egui::Popup::is_any_open(&ctx));
    assert!(app.keyboard_blocked(&ctx));

    run(&mut app, key(Key::W, Modifiers::COMMAND).to_vec());
    assert_eq!(app.panes[0].tabs.len(), 2);
    assert!(egui::Popup::is_any_open(&ctx));
    run(&mut app, key(Key::Escape, Modifiers::NONE).to_vec());
    assert!(!egui::Popup::is_any_open(&ctx));
    assert!(app.tab().selected.contains(&file));
    run(&mut app, key(Key::W, Modifiers::COMMAND).to_vec());
    assert_eq!(app.panes[0].tabs.len(), 1);
}

#[test]
fn tab_actions_duplicate_and_reopen_exact_locations_and_persist_pins() {
    let ctx = egui::Context::default();
    let root = tempfile::tempdir().unwrap();
    let local: Location = root.path().to_path_buf().into();
    let remote = Location::Sftp {
        endpoint: Endpoint {
            host: "127.0.0.1".into(),
            port: 2222,
            user: "test".into(),
            key: Some(root.path().join("key with spaces")),
            jump: None,
        },
        path: "/папка/two words".into(),
    };
    let open = |storage: &MemoryStorage, start| {
        let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
        cc.storage = Some(storage);
        PawCommander::new(&cc, start)
    };
    let mut storage = MemoryStorage::default();
    let mut app = open(&storage, Some(root.path().into()));
    app.panes[0].tabs.push(Tab::new(remote.clone(), &ctx));
    let local_id = app.tab().id;
    let remote_id = app.panes[0].tabs[1].id;

    app.action(Action::TogglePinTab(0, 1), &ctx);
    assert_eq!(app.tab().id, local_id);
    assert_eq!(app.panes[0].tabs[0].id, remote_id);
    assert!(app.panes[0].tabs[0].pinned);
    assert!(!app.panes[0].can_close_tab(0));
    app.action(Action::CloseTab(0, 0), &ctx);
    app.panes[0].select_tab(0);
    app.run_shortcut(Command::CloseTab, &ctx);
    assert_eq!(app.panes[0].tabs.len(), 2);
    assert!(app.panes[0].closed_tabs.is_empty());

    app.action(Action::DuplicateTab(0, 0), &ctx);
    let duplicate_id = app.tab().id;
    assert_ne!(duplicate_id, remote_id);
    assert_eq!(app.tab().path, remote);
    assert!(!app.tab().pinned);
    app.action(Action::CloseTab(0, 1), &ctx);
    app.action(Action::CloseTab(0, 1), &ctx);
    app.action(Action::CloseTab(0, 0), &ctx);
    app.action(Action::ReopenTab(1), &ctx);
    assert_eq!(app.active, 0);
    assert_eq!(app.panes[1].tabs.len(), 1);

    app.run_shortcut(Command::ReopenTab, &ctx);
    assert_eq!(app.tab().path, local);
    app.run_shortcut(Command::ReopenTab, &ctx);
    assert_eq!(app.panes[0].current, 1);
    assert_eq!(app.tab().path, remote);
    assert_ne!(app.tab().id, duplicate_id);
    assert!(app.panes[0].closed_tabs.is_empty());
    app.action(Action::CloseTab(0, 1), &ctx);

    eframe::App::save(&mut app, &mut storage);
    let restored = open(&storage, None);
    assert!(restored.panes[0].closed_tabs.is_empty());
    assert_eq!(restored.panes[0].pinned_count(), 1);
    assert_eq!(restored.panes[0].tabs[0].path, remote);
    assert_eq!(restored.panes[0].tabs[1].path, local);
    assert_eq!(restored.panes[1].pinned_count(), 0);
    let explicit_start = open(&storage, Some(root.path().into()));
    assert!(!explicit_start.tab().pinned);
    assert_eq!(explicit_start.tab().path, local);
}

#[test]
fn remote_tab_history_and_new_tab_keep_backend() {
    let ctx = egui::Context::default();
    let root = tempfile::tempdir().unwrap();
    let local = Location::Local(root.path().into());
    let remote = Location::Sftp {
        endpoint: Endpoint {
            host: "127.0.0.1".into(),
            port: 0,
            user: "test".into(),
            key: None,
            jump: None,
        },
        path: "/".into(),
    };
    let mut tab = Tab::new(local.clone(), &ctx);
    tab.navigate(remote.clone(), &ctx);
    tab.navigate(remote.join("sub"), &ctx);
    tab.history_go(-1, &ctx);
    assert_eq!(tab.path, remote);
    tab.history_go(-1, &ctx);
    assert_eq!(tab.path, local);
    tab.history_go(1, &ctx);
    let duplicate = Tab::new(tab.path.clone(), &ctx);
    assert_eq!(duplicate.path, remote);
    assert_eq!(duplicate.address, remote.display());
}
