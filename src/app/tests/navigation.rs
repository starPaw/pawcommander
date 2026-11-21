use super::*;

#[test]
fn directory_tree_targets_the_active_pane_and_preserves_current_selection() {
    use egui::accesskit::{Action as AxAction, ActionRequest, Role, Toggled};

    let fixture = tempfile::tempdir().unwrap();
    let parent = Location::Local(fixture.path().join("Tree parent"));
    let folder = parent.join("Current folder");
    let file = folder.join("selection.txt");
    std::fs::create_dir_all(folder.path()).unwrap();
    std::fs::write(file.path(), b"example").unwrap();

    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut frame = eframe::Frame::_new_kittest();
    let mut app = PawCommander::new(&cc, Some(fixture.path().into()));
    app.settings.split = true;
    app.action(Action::Navigate(1, folder.clone()), &ctx);
    wait_for_directory(app.tab_mut());
    app.tab_mut().selected.insert(file.clone());

    let mut time = 0.0;
    let mut run = |app: &mut PawCommander, events| {
        time += 0.2;
        ctx.run_test_ui(
            egui::RawInput {
                events,
                time: Some(time),
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1440.0, 860.0),
                )),
                ..Default::default()
            },
            |ui| {
                eframe::App::logic(app, ui.ctx(), &mut frame);
                eframe::App::ui(app, ui, &mut frame);
            },
        )
        .platform_output
        .accesskit_update
        .unwrap()
    };
    let click = |target| {
        vec![egui::Event::AccessKitActionRequest(ActionRequest {
            action: AxAction::Click,
            target_tree: egui::accesskit::TreeId::ROOT,
            target_node: target,
            data: None,
        })]
    };
    let sidebar = run(&mut app, vec![]);
    let (tree_title, _) = sidebar
        .nodes
        .iter()
        .find(|(_, node)| matches!(node.label(), Some("Folder tree" | "Дерево папок")))
        .unwrap();
    run(&mut app, click(*tree_title));
    let visible = run(&mut app, vec![]);
    let tree_row = |label, selected| {
        *visible
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == Role::Button
                    && node.label() == Some(label)
                    && node.toggled() == Some(selected)
            })
            .map(|(id, _)| id)
            .unwrap()
    };
    let current_row = tree_row("Current folder", Toggled::True);
    let parent_row = tree_row("Tree parent", Toggled::False);
    let history_length = app.tab().history.len();
    run(&mut app, click(current_row));
    assert_eq!(app.tab().selected, BTreeSet::from([file]));
    assert_eq!(app.tab().history.len(), history_length);

    run(&mut app, click(parent_row));
    assert_eq!(app.active, 1);
    assert_eq!(app.tab().path, parent);
    assert_eq!(app.tab().history.len(), history_length + 1);
    assert_eq!(app.panes[0].tab().path.local(), Some(fixture.path()));
}

#[test]
fn folder_jump_uses_successful_visits_and_opens_in_the_active_pane() {
    use egui::{Event, Key, Modifiers};

    let root = tempfile::tempdir().unwrap();
    // Include an ancestor that also matches the fuzzy query, independently of TEMP.
    let base: Location = root.path().join("PawCommanderQA").into();
    let target = base.join("PawCommander");
    let missing = base.join("Missing");
    std::fs::create_dir(base.path()).unwrap();
    std::fs::create_dir(target.path()).unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut frame = eframe::Frame::_new_kittest();
    let mut app = PawCommander::new(&cc, Some(base.path().into()));
    wait_for_directory(app.tab_mut());
    app.poll(&ctx);
    app.action(Action::Navigate(0, target.clone()), &ctx);
    wait_for_directory(app.tab_mut());
    app.poll(&ctx);
    app.action(Action::Navigate(0, missing.clone()), &ctx);
    wait_for_directory(app.tab_mut());
    app.poll(&ctx);
    assert!(!app.settings.folder_history.ranked("").contains(&&missing));

    let mut storage = MemoryStorage::default();
    eframe::App::save(&mut app, &mut storage);
    let restored: Settings = eframe::get_value(&storage, "pawcommander.settings").unwrap();
    assert_eq!(restored.folder_history.ranked("pwcm"), vec![&target, &base]);

    app.active = 1;
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
    let press = |key, modifiers| {
        [true, false].map(|pressed| Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers,
        })
    };
    let command_shift = Modifiers::COMMAND | Modifiers::SHIFT;
    run(&mut app, press(Key::J, command_shift).to_vec());
    assert!(app.folder_jump.is_some());
    run(&mut app, press(Key::Escape, Modifiers::NONE).to_vec());
    assert!(app.folder_jump.is_none());
    assert_eq!(app.tab().path, base);

    run(&mut app, press(Key::P, command_shift).to_vec());
    run(&mut app, vec![]);
    run(
        &mut app,
        vec![Event::Text(Command::FolderJump.label().into())],
    );
    run(&mut app, press(Key::Enter, Modifiers::NONE).to_vec());
    assert!(app.command_palette.is_none());
    assert!(app.folder_jump.is_some());
    assert_eq!(app.tab().path, base);
    run(&mut app, vec![]);
    run(&mut app, vec![]);
    run(&mut app, vec![Event::Text("pwcm".into())]);
    run(&mut app, press(Key::Enter, Modifiers::NONE).to_vec());
    assert!(app.folder_jump.is_none());
    assert_eq!(app.active, 1);
    assert_eq!(app.tab().path, target);
    assert_eq!(app.panes[0].tab().path, missing);
}

#[test]
fn folder_jump_seeds_visible_restored_tabs_once() {
    let root = tempfile::tempdir().unwrap();
    let base: Location = root.path().to_path_buf().into();
    let left = base.join("Left current");
    let right = base.join("Right current");
    let background = base.join("Background tab");
    for location in [&left, &right, &background] {
        std::fs::create_dir(location.path()).unwrap();
    }
    let ctx = egui::Context::default();
    let mut storage = MemoryStorage::default();
    let legacy = Settings {
        locations: vec![vec![left.clone(), background.clone()], vec![right.clone()]],
        ..Settings::default()
    };
    eframe::set_value(&mut storage, "pawcommander.settings", &legacy);
    let open = |storage: &MemoryStorage| {
        let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
        cc.storage = Some(storage);
        let mut app = PawCommander::new(&cc, None);
        for pane in &mut app.panes {
            for tab in &mut pane.tabs {
                wait_for_directory(tab);
            }
        }
        app.poll(&ctx);
        app
    };
    let mut app = open(&storage);
    assert_eq!(
        app.settings.folder_history.ranked("current"),
        vec![&right, &left]
    );
    assert!(app.settings.folder_history.ranked("background").is_empty());
    app.panes[0].select_tab(1);
    app.poll(&ctx);
    assert_eq!(
        app.settings.folder_history.ranked("background"),
        vec![&background]
    );

    let before = serde_json::to_string(&app.settings.folder_history).unwrap();
    let tab = app.panes[0].tab_mut();
    tab.auto_refresh(&ctx, tab.next_auto_refresh);
    assert!(tab.automatic_loading());
    wait_for_directory(tab);
    app.poll(&ctx);
    assert_eq!(
        serde_json::to_string(&app.settings.folder_history).unwrap(),
        before
    );
    eframe::App::save(&mut app, &mut storage);
    let reopened = open(&storage);
    assert_eq!(
        serde_json::to_string(&reopened.settings.folder_history).unwrap(),
        before
    );
}

#[test]
fn folder_jump_records_startup_visit_when_left_navigates_before_completion() {
    let root = tempfile::tempdir().unwrap();
    let initial: Location = root.path().join("initial").into();
    let next: Location = root.path().join("next").into();
    for path in [&initial, &next] {
        std::fs::create_dir(path.path()).unwrap();
    }
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(initial.path().into()));
    app.settings.folder_history.record(&initial);
    // Supersede the left read before the app observes either completion.
    app.panes[0].tab_mut().navigate(next.clone(), &ctx);
    for pane in &mut app.panes {
        wait_for_directory(pane.tab_mut());
    }
    app.poll(&ctx);
    let recorded = serde_json::to_value(&app.settings.folder_history).unwrap();
    let visits = recorded["visits"].as_array().unwrap();
    let count = |path: &Location| {
        visits
            .iter()
            .find(|visit| visit["location"] == serde_json::to_value(path).unwrap())
            .unwrap()["count"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(
        count(&initial),
        2,
        "right successful opening records the launch once"
    );
    assert_eq!(count(&next), 1);
    for _ in 0..3 {
        app.poll(&ctx);
    }
    assert_eq!(
        serde_json::to_value(&app.settings.folder_history).unwrap(),
        recorded
    );
}

#[test]
fn folder_jump_counts_both_startup_completion_orders_once() {
    for first in [0, 1] {
        let root = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        for pane in &mut app.panes {
            wait_for_directory(pane.tab_mut());
        }
        // Hold one successful result until after the other pane is observed.
        app.panes[1 - first].tab_mut().has_complete_listing = false;
        app.poll(&ctx);
        let once = serde_json::to_value(&app.settings.folder_history).unwrap();
        assert_eq!(once["visits"].as_array().unwrap().len(), 1);
        assert_eq!(once["visits"][0]["count"], 1);
        app.panes[1 - first].tab_mut().has_complete_listing = true;
        app.poll(&ctx);
        assert_eq!(
            serde_json::to_value(&app.settings.folder_history).unwrap(),
            once
        );
    }
}

#[test]
fn folder_jump_failed_startup_does_not_consume_first_success() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(missing.clone()));
    for pane in &mut app.panes {
        wait_for_directory(pane.tab_mut());
    }
    app.poll(&ctx);
    assert!(app.settings.folder_history.ranked("").is_empty());
    std::fs::create_dir(&missing).unwrap();
    app.panes[1].tab_mut().reload(&ctx);
    wait_for_directory(app.panes[1].tab_mut());
    app.poll(&ctx);
    let once = serde_json::to_value(&app.settings.folder_history).unwrap();
    assert_eq!(once["visits"].as_array().unwrap().len(), 1);
    assert_eq!(once["visits"][0]["count"], 1);
}

#[test]
fn sftp_file_double_click_and_zed_context_menu_keep_the_clicked_path() {
    fn text_rect(shape: &egui::epaint::Shape) -> Option<egui::Rect> {
        match shape {
            egui::epaint::Shape::Text(text) if text.galley.job.text == "Open in Zed" => {
                Some(text.galley.rect.translate(text.pos.to_vec2()))
            }
            egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(text_rect),
            _ => None,
        }
    }
    for (context_menu, zed_available, checkbox) in [
        (false, true, false),
        (true, true, false),
        (true, false, false),
        (false, true, true),
    ] {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
        tab.loading = None;
        let path = Location::Sftp {
            endpoint: Endpoint {
                host: "example.invalid".into(),
                port: 2222,
                user: "test".into(),
                key: None,
                jump: None,
            },
            path: "/project/.env".into(),
        };
        tab.entries.replace(vec![FileEntry {
            path: path.clone(),
            name: ".env".into(),
            is_dir: false,
            is_symlink: false,
            size: 12,
            modified: None,
            metadata: Default::default(),
        }]);
        let mut time = 0.0;
        let mut render = |events| {
            let mut actions = vec![];
            time += 0.02;
            let output = ctx.run_test_ui(
                egui::RawInput {
                    time: Some(time),
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(600.0, 400.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        entry_widget(
                            ui,
                            &mut tab,
                            0,
                            &[0],
                            EntryOptions {
                                columns: None,
                                thumbnails: None,
                                pane: 1,
                                split: true,
                                grid: false,
                                folder_columns: false,
                                compact: false,
                                idle: true,
                                zed_available,
                            },
                            &mut actions,
                            &mut DropTargets::default(),
                        );
                    });
                },
            );
            (actions, output)
        };
        // The leading checkbox toggles selection; double-click the filename to open.
        let pos = egui::pos2(if checkbox { 24.0 } else { 90.0 }, 24.0);
        let button = |button, pressed, pos| egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        render(vec![egui::Event::PointerMoved(pos)]);
        if context_menu {
            render(vec![button(egui::PointerButton::Secondary, true, pos)]);
            render(vec![button(egui::PointerButton::Secondary, false, pos)]);
            let (_, output) = render(vec![]);
            let rect = output
                .shapes
                .iter()
                .find_map(|s| text_rect(&s.shape))
                .expect("file menu exposes Open in Zed");
            let menu_pos = rect.center();
            render(vec![
                egui::Event::PointerMoved(menu_pos),
                button(egui::PointerButton::Primary, true, menu_pos),
            ]);
            let (actions, _) = render(vec![button(egui::PointerButton::Primary, false, menu_pos)]);
            assert!(
                actions
                    .iter()
                    .any(|a| matches!(a, Action::OpenInZed(1, p) if p == &path))
                    == zed_available
            );
        } else {
            render(vec![button(egui::PointerButton::Primary, true, pos)]);
            render(vec![button(egui::PointerButton::Primary, false, pos)]);
            render(vec![button(egui::PointerButton::Primary, true, pos)]);
            let (actions, _) = render(vec![button(egui::PointerButton::Primary, false, pos)]);
            assert_eq!(
                actions
                    .iter()
                    .any(|a| matches!(a, Action::Open(1, p) if p == &path)),
                !checkbox
            );
            if checkbox {
                assert!(
                    tab.selected.is_empty(),
                    "two checkbox clicks toggle selection on and off"
                );
            }
        }
    }
}
