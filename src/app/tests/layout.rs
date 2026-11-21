use super::*;

#[test]
fn empty_pane_space_activates_without_changing_selection() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("keep.txt"), b"selection stays").unwrap();
    for orientation in [PaneOrientation::SideBySide, PaneOrientation::Stacked] {
        for mode in [FolderMode::List, FolderMode::Grid, FolderMode::Columns] {
            let ctx = egui::Context::default();
            let cc = eframe::CreationContext::_new_kittest(ctx.clone());
            let mut app = PawCommander::new(&cc, Some(root.path().into()));
            let mut frame = eframe::Frame::_new_kittest();
            app.settings.preview = false;
            app.settings.pane_layout.sidebar_visible = false;
            app.settings.pane_layout.orientation = orientation;
            for index in 0..2 {
                wait_for_directory(app.panes[index].tab_mut());
                app.change_folder_view(index, FolderViewChange::Mode(mode));
                let path = app.panes[index].tab().path.join("keep.txt");
                app.panes[index].tab_mut().selected.insert(path);
            }
            let before = [
                app.panes[0].tab().selected.clone(),
                app.panes[1].tab().selected.clone(),
            ];
            let mut draw = |app: &mut PawCommander, events| {
                let _ = ctx.run_test_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1200.0, 800.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ui| eframe::App::ui(app, ui, &mut frame),
                );
            };
            draw(&mut app, vec![]);
            let empty = match orientation {
                PaneOrientation::SideBySide => egui::pos2(1100.0, 600.0),
                PaneOrientation::Stacked => egui::pos2(1000.0, 680.0),
            };
            draw(
                &mut app,
                vec![
                    egui::Event::PointerMoved(empty),
                    egui::Event::PointerButton {
                        pos: empty,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            assert_eq!(
                app.active, 1,
                "empty {orientation:?}/{mode:?} pane should activate on press"
            );
            draw(
                &mut app,
                vec![egui::Event::PointerButton {
                    pos: empty,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            assert_eq!(app.panes[0].tab().selected, before[0]);
            assert_eq!(app.panes[1].tab().selected, before[1]);
        }
    }
}

#[test]
fn pane_layout_defaults_and_persists_through_settings() {
    let directory = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let mut storage = MemoryStorage::default();
    eframe::Storage::set_string(
        &mut storage,
        PawCommander::SETTINGS_KEY,
        "(split:false,preview:false)".into(),
    );
    let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
    cc.storage = Some(&storage);
    let mut app = PawCommander::new(&cc, Some(directory.path().into()));
    assert!(!app.settings.split);
    assert!(!app.settings.preview);
    assert_eq!(
        app.settings.pane_layout.orientation,
        PaneOrientation::SideBySide
    );
    assert_eq!(app.settings.pane_layout.fractions, [0.5, 0.5]);
    assert!(app.settings.pane_layout.sidebar_visible);

    app.settings.pane_layout.orientation = PaneOrientation::Stacked;
    app.settings.pane_layout.fractions = [0.35, 0.6];
    app.settings.pane_layout.sidebar_visible = false;
    eframe::App::save(&mut app, &mut storage);
    let mut cc = eframe::CreationContext::_new_kittest(ctx);
    cc.storage = Some(&storage);
    let reopened = PawCommander::new(&cc, None);
    assert_eq!(
        reopened.settings.pane_layout.orientation,
        PaneOrientation::Stacked
    );
    assert_eq!(reopened.settings.pane_layout.fractions, [0.35, 0.6]);
    assert!(!reopened.settings.pane_layout.sidebar_visible);
    assert!(!reopened.settings.split);
    assert!(!reopened.settings.preview);
}

#[test]
fn pane_layout_drag_is_bounded_and_remembers_both_orientations() {
    let ctx = egui::Context::default();
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 720.0));
    let draw = |layout: &mut PaneLayout, events| {
        let mut result = [egui::Rect::NOTHING; 2];
        let output = ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    result = layout.rects(ui, ui.available_rect_before_wrap());
                });
            },
        );
        (result, output.platform_output.cursor_icon)
    };
    let mut layout = PaneLayout::default();
    for orientation in [PaneOrientation::SideBySide, PaneOrientation::Stacked] {
        layout.orientation = orientation;
        let axis = orientation.axis();
        let (panes, _) = draw(&mut layout, vec![]);
        assert_eq!(panes[0].size(), panes[1].size());
        assert_eq!(panes[1].min[axis] - panes[0].max[axis], 8.0);
        let mut center = panes[0].center();
        center[axis] = panes[0].max[axis] + 4.0;
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            pressed,
            button: egui::PointerButton::Primary,
            modifiers: egui::Modifiers::NONE,
        };
        let (_, cursor) = draw(&mut layout, vec![egui::Event::PointerMoved(center)]);
        assert_eq!(
            cursor,
            if axis == 0 {
                egui::CursorIcon::ResizeHorizontal
            } else {
                egui::CursorIcon::ResizeVertical
            }
        );
        draw(&mut layout, vec![button(center, true)]);
        let mut edge = center;
        edge[axis] = screen.max[axis] - 1.0;
        let (resized, _) = draw(&mut layout, vec![egui::Event::PointerMoved(edge)]);
        draw(&mut layout, vec![button(edge, false)]);
        assert!(resized[0].size()[axis] > panes[0].size()[axis]);
        assert!(resized[1].size()[axis] >= [180.0, 220.0][axis]);
        assert_eq!(resized[1].min[axis] - resized[0].max[axis], 8.0);
    }
    assert!(layout.fractions.into_iter().all(|fraction| fraction > 0.5));
    layout.orientation = PaneOrientation::SideBySide;
    let (panes, _) = draw(&mut layout, vec![]);
    assert!(panes[0].width() > panes[1].width());
}

#[test]
fn custom_columns_scroll_without_losing_rows_or_compact_headers() {
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let folder = tempfile::tempdir().unwrap();
    let mut app = PawCommander::new(&cc, Some(folder.path().into()));
    let mut frame = eframe::Frame::_new_kittest();
    app.settings.preview = false;
    app.settings.pane_layout.sidebar_visible = false;
    app.settings.list_columns = serde_json::from_value(serde_json::json!({
        "custom": true,
        "name_width": 600,
        "columns": [
            {"column":"Extension", "width":100, "visible":true},
            {"column":"Permissions", "width":100, "visible":true}
        ]
    }))
    .unwrap();
    app.settings.list_columns.validate();
    let mut storage = MemoryStorage::default();
    eframe::App::save(&mut app, &mut storage);
    let restored = eframe::get_value::<Settings>(&storage, PawCommander::SETTINGS_KEY).unwrap();
    assert_eq!(restored.list_columns, app.settings.list_columns);
    let path = app.tab().path.clone();
    let row = path.join("item040.txt");
    let entries = (0..100)
        .map(|index| {
            let name = format!("item{index:03}.txt");
            FileEntry {
                path: path.join(&name),
                name,
                is_dir: false,
                is_symlink: false,
                size: 1,
                modified: None,
                metadata: Default::default(),
            }
        })
        .collect();
    let tab = app.tab_mut();
    tab.loading = None;
    tab.has_complete_listing = true;
    tab.entries.replace(entries);
    tab.scroll_to = Some(row.clone());
    let row_id = tab.entry_id(&row);
    let mut time = 0.0;
    let mut draw = |app: &mut PawCommander, events| {
        let mut response = None;
        time += 0.08;
        let output = ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(720.0, 430.0),
                )),
                events,
                time: Some(time),
                ..Default::default()
            },
            |ui| {
                let ctx = &ui.ctx().clone();
                eframe::App::ui(app, ui, &mut frame);
                response = ctx.read_response(row_id);
            },
        );
        (output, response)
    };
    let scroll = |delta| egui::Event::MouseWheel {
        phase: egui::TouchPhase::Move,
        source: egui::MouseWheelSource::Unknown,
        unit: egui::MouseWheelUnit::Point,
        delta,
        modifiers: egui::Modifiers::NONE,
    };
    let (_, initial) = draw(&mut app, vec![]);
    let initial = initial.unwrap();
    let y_before = app.tab().scroll_offset;
    assert!(initial.interact_rect.is_positive());
    assert!(app.tab().rendered_rows.len() < 20);
    draw(
        &mut app,
        vec![
            egui::Event::PointerMoved(initial.interact_rect.center()),
            scroll(egui::vec2(-700.0, 0.0)),
        ],
    );
    let (_, scrolled) = draw(&mut app, vec![]);
    let scrolled = scrolled.unwrap();
    assert!(
        scrolled.rect.left() < initial.rect.left(),
        "{initial:?} -> {scrolled:?}"
    );
    assert_eq!(app.tab().scroll_offset, y_before);
    draw(&mut app, vec![scroll(egui::vec2(0.0, -110.0))]);
    assert!(app.tab().scroll_offset > y_before);

    app.settings.pane_layout.orientation = PaneOrientation::Stacked;
    app.tab_mut().scroll_to = Some(row);
    let (compact, response) = draw(&mut app, vec![]);
    assert!(response.unwrap().interact_rect.is_positive());
    assert!(compact.shapes.iter().any(|shape| {
        matches!(&shape.shape, egui::Shape::Text(text) if matches!(
            text.galley.text(), "Extension" | "Расширение"
        ) && shape.clip_rect.contains(text.pos))
    }));
}

#[test]
fn pane_layout_preserves_selection_and_scroll_at_minimum_size() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..100 {
        std::fs::write(root.path().join(format!("file{index:03}")), b"test").unwrap();
    }
    for grid in [false, true] {
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        let mut frame = eframe::Frame::_new_kittest();
        let selected = app.tab().path.join("file040");
        let mut draw = |app: &mut PawCommander, height, events| {
            let mut input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, height),
                )),
                events,
                ..Default::default()
            };
            eframe::App::raw_input_hook(app, &ctx, &mut input);
            let mut rows = [None, None];
            let output = ctx.run_test_ui(input, |ui| {
                let ctx = &ui.ctx().clone();
                eframe::App::ui(app, ui, &mut frame);
                for (index, pane) in app.panes.iter().enumerate() {
                    rows[index] = ctx.read_response(pane.tab().entry_id(&selected));
                }
            });
            (output, rows)
        };
        wait_for_directory(app.panes[0].tab_mut());
        wait_for_directory(app.panes[1].tab_mut());
        app.active = 1;
        app.settings.pane_grid = Some([false, grid]);
        let tab_id = app.tab().id;
        app.tab_mut().scroll_to = Some(selected.clone());
        app.tab_mut().cursor = Some(selected.clone());
        app.tab_mut().selected.insert(selected.clone());
        let (_, rows) = draw(&mut app, 400.0, vec![]);
        let scope = app.tab().file_scroll_scope;
        let row = app.tab().entry_id(&selected);
        assert!(rows[1].as_ref().unwrap().interact_rect.is_positive());
        ctx.memory_mut(|memory| memory.request_focus(row));

        app.settings.pane_layout.orientation = PaneOrientation::Stacked;
        app.settings.pane_layout.sidebar_visible = false;
        let (_, rows) = draw(&mut app, 400.0, vec![]);
        assert_eq!(app.active, 1);
        assert_eq!(app.tab().id, tab_id);
        assert_eq!(app.tab().file_scroll_scope, scope);
        assert_eq!(app.tab().selected, BTreeSet::from([selected.clone()]));
        assert_eq!(app.file_row_focus(&ctx), Some((1, &selected)));
        assert!(rows[1].as_ref().unwrap().interact_rect.is_positive());

        draw(
            &mut app,
            400.0,
            vec![egui::Event::Key {
                key: egui::Key::ArrowDown,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert_eq!(app.active, 1);
        assert_eq!(
            app.tab().selected,
            BTreeSet::from([app.tab().path.join("file041")])
        );

        app.settings.pane_layout.sidebar_visible = true;
        app.settings.pane_grid = Some([grid; 2]);
        for (height, fraction) in [
            (400.0, 0.0),
            (400.0, 1.0),
            (560.0, 0.0),
            (560.0, 1.0),
            (670.0, 0.5),
            (690.0, 0.5),
        ] {
            app.settings.pane_layout.fractions[1] = fraction;
            for pane in &mut app.panes {
                pane.tab_mut().scroll_to = Some(selected.clone());
            }
            let (output, rows) = draw(&mut app, height, vec![]);
            for row in rows {
                let row = row.unwrap();
                assert!(
                    row.interact_rect.height() >= COMPACT_FILE_ROW_HEIGHT - 1.0,
                    "clickable file row at height {height}, fraction {fraction}, grid {grid}: {row:?}"
                );
                let labels = output
                    .shapes
                    .iter()
                    .filter_map(|clipped| match &clipped.shape {
                        egui::Shape::Text(text) if text.galley.text() == "file040" => {
                            Some((text.visual_bounding_rect(), clipped.clip_rect))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert!(labels.iter().any(|(label, clip)| {
                    row.interact_rect.contains_rect(*label) && clip.contains_rect(*label)
                }), "filename must be visible in both panes at height {height}, fraction {fraction}, grid {grid}. Row {row:?}, labels {labels:?}");
            }
        }
    }
}

#[test]
fn pane_layout_accepts_tab_drops_between_stacked_panes() {
    let root = tempfile::tempdir().unwrap();
    let top: Location = root.path().join("top").into();
    let bottom: Location = root.path().join("bottom").into();
    std::fs::create_dir(top.path()).unwrap();
    std::fs::create_dir(bottom.path()).unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(bottom.path().into()));
    app.settings.pane_layout.orientation = PaneOrientation::Stacked;
    let tab = Tab::new(top, &ctx);
    let id = tab.id;
    app.panes[0].tabs.push(tab);

    let mut frame = eframe::Frame::_new_kittest();
    let mut draw = |app: &mut PawCommander, events| {
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 700.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                eframe::App::logic(app, ui.ctx(), &mut frame);
                eframe::App::ui(app, ui, &mut frame);
            },
        )
    };
    let output = draw(&mut app, vec![]);
    let mut top_tab = None;
    let mut bottom_tab = None;
    for clipped in &output.shapes {
        if let egui::Shape::Text(text) = &clipped.shape {
            let center = text.pos + text.galley.size() * 0.5;
            if text.galley.text() == "top" {
                top_tab = Some(center);
            }
            if text.galley.text() == "bottom"
                && bottom_tab.is_none_or(|previous: egui::Pos2| previous.y < center.y)
                && clipped.clip_rect.contains(center)
            {
                bottom_tab = Some(center);
            }
        }
    }
    let start = top_tab.unwrap();
    let end = bottom_tab.unwrap();
    assert!(start.y < end.y);
    let button = |pos, pressed| egui::Event::PointerButton {
        pos,
        pressed,
        button: egui::PointerButton::Primary,
        modifiers: egui::Modifiers::NONE,
    };
    draw(
        &mut app,
        vec![egui::Event::PointerMoved(start), button(start, true)],
    );
    draw(&mut app, vec![egui::Event::PointerMoved(end)]);
    draw(&mut app, vec![button(end, false)]);
    assert_eq!(app.panes[0].tabs.len(), 1);
    assert_eq!(app.panes[1].tab().id, id);
    assert_eq!(app.active, 1);
}

#[test]
fn pane_views_migrate_and_persist_independently() {
    let mut settings = Settings {
        grid: true,
        ..Default::default()
    };
    assert!(settings.grid_for(0) && settings.grid_for(1));
    settings.pane_grid = Some([false, true]);
    let mut storage = MemoryStorage::default();
    eframe::set_value(&mut storage, "views", &settings);
    let restored: Settings = eframe::get_value(&storage, "views").unwrap();
    assert!(!restored.grid_for(0));
    assert!(restored.grid_for(1));
}

#[test]
fn folder_columns_keyboard_dispatches_in_pane_order_and_preserves_folder_overrides() {
    let root = tempfile::tempdir().unwrap();
    let left = root.path().join("left");
    let right = root.path().join("right");
    let child = right.join("child");
    let leaf = child.join("leaf");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir_all(&leaf).unwrap();
    let ctx = egui::Context::default();
    let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(left.clone()));
    app.action(Action::Navigate(1, right.clone().into()), &ctx);
    wait_for_directory(app.panes[1].tab_mut());
    app.action(
        app.folder_view_action(1, FolderViewChange::Mode(FolderMode::Columns)),
        &ctx,
    );
    app.active = 0;

    let key = |key| egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    let run = |app: &mut PawCommander, events| {
        let mut input = egui::RawInput {
            events,
            ..Default::default()
        };
        eframe::App::raw_input_hook(app, &ctx, &mut input);
        let _ = ctx.run_test_ui(input, |ui| app.keyboard(ui.ctx()));
    };
    let stale = app.panes[1].tab().column_action(1, ColumnNavigation::Left);
    run(
        &mut app,
        vec![
            key(egui::Key::Tab),
            key(egui::Key::ArrowDown),
            key(egui::Key::ArrowRight),
        ],
    );
    assert_eq!(app.active, 1);
    assert_eq!(app.panes[0].tab().path.path(), left);
    assert_eq!(app.tab().path.path(), child);
    assert_eq!(app.folder_view(1).mode, FolderMode::Columns);
    app.action(stale, &ctx);
    assert_eq!(
        app.tab().path.path(),
        child,
        "stale rendered actions cannot navigate"
    );
    wait_for_directory(app.tab_mut());
    let mut storage = MemoryStorage::default();
    eframe::App::save(&mut app, &mut storage);
    cc.storage = Some(&storage);
    let reopened = PawCommander::new(&cc, None);
    assert_eq!(reopened.panes[1].tab().path.path(), child);
    assert_eq!(reopened.folder_view(1).mode, FolderMode::Columns);

    run(&mut app, vec![key(egui::Key::ArrowLeft)]);
    wait_for_directory(app.tab_mut());
    assert_eq!(app.tab().path.path(), right);
    assert_eq!(app.selected(), vec![Location::Local(child.clone())]);
    let legacy: FolderViewOverrides = serde_json::from_value(serde_json::json!({
        "grid": true,
    }))
    .unwrap();
    app.folder_views.insert(leaf.clone().into(), legacy);
    run(&mut app, vec![key(egui::Key::ArrowRight)]);
    assert_eq!(app.tab().path.path(), child);
    assert_eq!(app.folder_view(1).mode, FolderMode::Columns);
    wait_for_directory(app.tab_mut());
    run(
        &mut app,
        vec![key(egui::Key::ArrowDown), key(egui::Key::ArrowRight)],
    );
    assert_eq!(app.tab().path.path(), leaf);
    assert_eq!(app.folder_view(1).mode, FolderMode::Grid);
    app.action(Action::Back, &ctx);
    assert_eq!(app.tab().path.path(), child);
    assert_eq!(app.folder_view(1).mode, FolderMode::Columns);
    app.action(Action::Forward, &ctx);
    assert_eq!(app.tab().path.path(), leaf);
    assert_eq!(app.folder_view(1).mode, FolderMode::Grid);
}

#[test]
fn folder_columns_pending_reveal_yields_to_progressive_user_selection() {
    let root = tempfile::tempdir().unwrap();
    let leave = root.path().join("left");
    let other = root.path().join("other");
    std::fs::create_dir(&leave).unwrap();
    std::fs::write(&other, b"file").unwrap();
    let ctx = egui::Context::default();
    let mut tab = Tab::new(leave.clone().into(), &ctx);
    tab.navigate_column(
        root.path().to_path_buf().into(),
        Some(leave.clone().into()),
        &ctx,
    );
    let (sender, load) = DirectoryLoad::channel(DirectoryLoadMode::Progressive);
    tab.loading = Some(load);
    let mut entries = fs_ops::read_directory(&tab.path, &|| false).unwrap();
    let file = entries.swap_remove(
        entries
            .iter()
            .position(|entry| entry.path.path() == other)
            .unwrap(),
    );
    sender.send(DirectoryRead::Batch(vec![file])).unwrap();
    assert!(tab.poll());
    tab.choose(0, &[0], egui::Modifiers::NONE);
    sender.send(DirectoryRead::Batch(entries)).unwrap();
    assert!(tab.poll());
    sender.send(DirectoryRead::Finished(Ok(None))).unwrap();
    wait_for_directory(&mut tab);
    assert_eq!(tab.selected, BTreeSet::from([other.into()]));
}

#[test]
fn folder_columns_current_rows_remain_visible_and_virtual_in_narrow_and_stacked_panes() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("first").join("second");
    std::fs::create_dir_all(&folder).unwrap();
    for index in 0..100 {
        std::fs::write(folder.join(format!("item{index:03}.txt")), b"test").unwrap();
    }
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(folder.clone()));
    let mut frame = eframe::Frame::_new_kittest();
    app.settings.preview = false;
    app.settings.pane_layout.sidebar_visible = false;
    app.action(
        app.folder_view_action(0, FolderViewChange::Mode(FolderMode::Columns)),
        &ctx,
    );
    wait_for_directory(app.tab_mut());
    let row: Location = folder.join("item040.txt").into();
    let row_id = app.tab().entry_id(&row);
    let mut time = 0.0;
    let mut draw = |app: &mut PawCommander, events| {
        time += 0.08;
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(390.0, 430.0),
            )),
            time: Some(time),
            events,
            ..Default::default()
        };
        eframe::App::raw_input_hook(app, &ctx, &mut input);
        let _ = ctx.run_test_ui(input, |ui| {
            eframe::App::logic(app, ui.ctx(), &mut frame);
            eframe::App::ui(app, ui, &mut frame);
        });
        ctx.read_response(row_id)
    };
    for orientation in [PaneOrientation::SideBySide, PaneOrientation::Stacked] {
        app.settings.pane_layout.orientation = orientation;
        app.tab_mut().scroll_to = Some(row.clone());
        let response = draw(&mut app, vec![]).expect("revealed row");
        assert!(response.interact_rect.width() >= 100.0, "{response:?}");
        assert!(response.rect.width() <= 240.0, "{response:?}");
        assert!(response.interact_rect.height() > 0.0, "{response:?}");
        assert!(app.tab().rendered_rows.len() < 20);

        if orientation == PaneOrientation::SideBySide {
            app.tab_mut().cursor = Some(row.clone());
            app.tab().focus_cursor(&ctx);
            draw(
                &mut app,
                vec![
                    egui::Event::PointerMoved(response.interact_rect.center()),
                    egui::Event::MouseWheel {
                        phase: egui::TouchPhase::Move,
                        source: egui::MouseWheelSource::Unknown,
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(1600.0, 0.0),
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            draw(&mut app, vec![]);
            assert!(
                app.tab().rendered_rows.is_empty(),
                "offscreen rows must leave focus order"
            );
            draw(
                &mut app,
                vec![egui::Event::Key {
                    key: egui::Key::ArrowDown,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            let next: Location = folder.join("item041.txt").into();
            assert_eq!(app.selected(), vec![next.clone()]);
            let response = ctx
                .read_response(app.tab().entry_id(&next))
                .expect("keyboard reveal");
            assert!(response.interact_rect.is_positive(), "{response:?}");
        }
    }
}

#[test]
fn folder_view_follows_location_across_panes_refresh_and_restart() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first");
    let other = root.path().join("other");
    for folder in [&first, &other] {
        std::fs::create_dir(folder).unwrap();
    }
    for (name, data) in [("small", "s"), ("big", "large"), (".hidden", "xx")] {
        std::fs::write(first.join(name), data).unwrap();
    }
    let hidden: Location = first.join(".hidden").into();
    let big: Location = first.join("big").into();
    let small: Location = first.join("small").into();
    let other: Location = other.into();
    let ctx = egui::Context::default();
    let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(first.clone()));

    app.action(
        app.folder_view_action(0, FolderViewChange::Mode(FolderMode::Grid)),
        &ctx,
    );
    app.action(
        app.folder_view_action(1, FolderViewChange::Density(Density::Compact)),
        &ctx,
    );
    app.action(
        app.folder_view_action(1, FolderViewChange::Sort(Sort::Size)),
        &ctx,
    );
    assert!(!app.folder_view(0).order.descending);
    app.action(
        app.folder_view_action(0, FolderViewChange::Sort(Sort::Size)),
        &ctx,
    );
    app.run_shortcut(Command::Hidden, &ctx);
    let expected = FolderView {
        mode: FolderMode::Grid,
        hidden: true,
        density: Density::Compact,
        order: FileOrder {
            key: Sort::Size,
            descending: true,
        },
    };
    assert_eq!(app.folder_view(0), expected);
    assert_eq!(app.folder_view(1), expected);
    app.action(Action::Refresh, &ctx);
    for pane in &mut app.panes {
        wait_for_directory(pane.tab_mut());
    }
    app.run_shortcut(Command::SelectAll, &ctx);
    assert_eq!(
        app.selected(),
        vec![big.clone(), hidden.clone(), small.clone()]
    );
    app.panes[1].tab_mut().selected.insert(hidden.clone());
    app.run_shortcut(Command::Hidden, &ctx);
    assert!(!app.panes[1].tab().selected.contains(&hidden));
    assert_eq!(app.selected(), vec![big, small]);
    app.run_shortcut(Command::Hidden, &ctx);

    let stale_sort = app.folder_view_action(0, FolderViewChange::Sort(Sort::Name));
    app.action(Action::Navigate(0, other.clone()), &ctx);
    app.action(stale_sort, &ctx);
    assert_eq!(app.folder_view(0), FolderView::default());
    app.action(Action::Back, &ctx);
    assert_eq!(app.folder_view(0), expected);
    app.action(Action::Forward, &ctx);
    assert_eq!(app.folder_view(0), FolderView::default());
    assert_eq!(app.folder_view(1), expected);
    app.action(Action::Navigate(1, other), &ctx);

    let mut storage = MemoryStorage::default();
    eframe::App::save(&mut app, &mut storage);
    cc.storage = Some(&storage);
    let mut reopened = PawCommander::new(&cc, None);
    assert_eq!(reopened.folder_view(0), FolderView::default());
    assert_eq!(reopened.folder_view(1), FolderView::default());
    reopened.action(Action::Back, &ctx);
    assert_eq!(reopened.folder_view(1), expected);
    reopened.action(Action::Navigate(0, first.into()), &ctx);
    assert_eq!(reopened.folder_view(0), expected);
}

#[test]
fn folder_view_storage_keeps_native_names_and_distinct_sftp_keys() {
    #[cfg(unix)]
    let native = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![b'x', 0xff])
    };
    #[cfg(windows)]
    let native = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[u16::from(b'x'), 0xd800])
    };
    let root = tempfile::tempdir().unwrap();
    let local: Location = root.path().join(&native).into();
    let remote_path = PathBuf::from("/folder").join(&native);
    let mut endpoint = Endpoint {
        host: "example.invalid".into(),
        port: 2022,
        user: "reader".into(),
        key: None,
        jump: None,
    };
    let remote = Location::Sftp {
        endpoint: endpoint.clone(),
        path: remote_path.clone(),
    };
    endpoint.key = Some(root.path().join(&native));
    let keyed = Location::Sftp {
        endpoint,
        path: remote_path,
    };

    let ctx = egui::Context::default();
    let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.action(Action::Navigate(0, local.clone()), &ctx);
    app.action(
        app.folder_view_action(0, FolderViewChange::Mode(FolderMode::Grid)),
        &ctx,
    );
    app.action(Action::Navigate(0, remote.clone()), &ctx);
    app.action(
        app.folder_view_action(0, FolderViewChange::Hidden(true)),
        &ctx,
    );
    app.action(Action::Navigate(0, keyed.clone()), &ctx);
    app.action(
        app.folder_view_action(0, FolderViewChange::Density(Density::Compact)),
        &ctx,
    );
    let mut storage = MemoryStorage::default();
    eframe::App::save(&mut app, &mut storage);
    cc.storage = Some(&storage);
    let mut reopened = PawCommander::new(&cc, Some(root.path().into()));
    for (location, expected) in [
        (
            local,
            FolderView {
                mode: FolderMode::Grid,
                ..Default::default()
            },
        ),
        (
            remote,
            FolderView {
                hidden: true,
                ..Default::default()
            },
        ),
        (
            keyed,
            FolderView {
                density: Density::Compact,
                ..Default::default()
            },
        ),
    ] {
        reopened.action(Action::Navigate(0, location), &ctx);
        assert_eq!(reopened.folder_view(0), expected);
    }
}

#[test]
fn folder_view_hiding_focused_row_cannot_delete_another_selected_file() {
    use egui::accesskit::{Action as AxAction, ActionRequest, NodeId};

    let root = tempfile::tempdir().unwrap();
    let hidden: Location = root.path().join(".hidden").into();
    let visible: Location = root.path().join("visible").into();
    for path in [&hidden, &visible] {
        std::fs::write(path.path(), "data").unwrap();
    }
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.run_shortcut(Command::Hidden, &ctx);
    wait_for_directory(app.tab_mut());
    app.tab_mut().selected.insert(visible.clone());
    let hidden_id = app.tab().entry_id(&hidden);
    let mut frame = eframe::Frame::_new_kittest();
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
        let _ = ctx.run_test_ui(input, |ui| {
            eframe::App::logic(app, ui.ctx(), &mut frame);
            eframe::App::ui(app, ui, &mut frame);
        });
    };
    run(&mut app, vec![]);
    run(
        &mut app,
        vec![egui::Event::AccessKitActionRequest(ActionRequest {
            action: AxAction::Focus,
            target_tree: egui::accesskit::TreeId::ROOT,
            target_node: NodeId(hidden_id.value()),
            data: None,
        })],
    );
    assert_eq!(app.file_row_focus(&ctx), Some((0, &hidden)));
    let key = |key, modifiers| egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    };
    run(
        &mut app,
        vec![
            key(egui::Key::H, egui::Modifiers::COMMAND),
            key(egui::Key::Delete, egui::Modifiers::NONE),
        ],
    );
    assert!(!app.folder_view(0).hidden);
    assert_eq!(app.selected(), vec![visible]);
    assert!(app.dialog.is_none());
}
