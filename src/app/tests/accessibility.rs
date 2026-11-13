use super::*;

#[test]
fn accessibility_tree_keeps_file_parent_focus_and_selection_across_views() {
    use egui::accesskit::{Action as AxAction, ActionRequest, Role, Toggled};

    let root = tempfile::tempdir().unwrap();
    let folder: Location = root.path().join("Folder").into();
    let file: Location = root.path().join("Note.txt").into();
    std::fs::create_dir(folder.path()).unwrap();
    std::fs::write(file.path(), "example").unwrap();
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut frame = eframe::Frame::_new_kittest();
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    wait_for_directory(app.tab_mut());
    wait_for_directory(app.panes[1].tab_mut());
    app.tab_mut().selected.insert(file.clone());
    app.panes[1].tab_mut().selected.insert(file.clone());
    let ax_id = |id: egui::Id| egui::accesskit::NodeId(id.value());
    let folder_id = ax_id(app.tab().entry_id(&folder));
    let file_id = ax_id(app.tab().entry_id(&file));
    let file_list_id = ax_id(app.tab().file_list_id());
    assert_ne!(file_id, ax_id(app.panes[1].tab().entry_id(&file)));
    assert_ne!(file_list_id, ax_id(app.panes[1].tab().file_list_id()));

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
        ctx.run(input, |ctx| eframe::App::update(app, ctx, &mut frame))
            .platform_output
            .accesskit_update
            .unwrap()
    };
    let request = |action| {
        vec![egui::Event::AccessKitActionRequest(ActionRequest {
            action,
            target: folder_id,
            data: None,
        })]
    };
    let key = |key| {
        [true, false]
            .into_iter()
            .map(move |pressed| egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            })
            .collect::<Vec<_>>()
    };
    let list = run(&mut app, vec![]);
    let row = list.nodes.iter().find(|(id, _)| *id == file_id).unwrap();
    assert_eq!(row.1.role(), Role::CheckBox);
    assert!(row.1.label().unwrap().starts_with("Note.txt, "));
    assert_eq!(row.1.toggled(), Some(Toggled::True));
    let assert_file_parent = |tree: &egui::accesskit::TreeUpdate| {
        let parents = tree
            .nodes
            .iter()
            .filter(|(_, node)| node.children().contains(&file_id))
            .collect::<Vec<_>>();
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0].0, file_list_id);
        assert_eq!(parents[0].1.role(), Role::List);
    };
    assert_file_parent(&list);
    app.tab_mut().browse_columns = true;
    assert_eq!(app.folder_view(0).mode, FolderMode::Columns);
    assert_file_parent(&run(&mut app, vec![]));
    app.tab_mut().browse_columns = false;
    assert_eq!(app.folder_view(0).mode, FolderMode::List);
    assert_file_parent(&run(&mut app, vec![]));

    let focused = run(&mut app, request(AxAction::Focus));
    assert_eq!(focused.focus, folder_id);
    assert_eq!(app.selected(), vec![file.clone()]);
    let mut switch_then_delete = key(egui::Key::Tab);
    switch_then_delete.extend(key(egui::Key::Delete));
    let switched = run(&mut app, switch_then_delete);
    assert_eq!(app.active, 1);
    assert_eq!(switched.focus, ax_id(app.tab().entry_id(&folder)));
    assert_eq!(app.selected(), vec![file.clone()]);
    assert!(
        app.dialog.is_none(),
        "Delete must respect the destination row after Tab"
    );
    run(&mut app, request(AxAction::Focus));
    run(&mut app, key(egui::Key::Delete));
    assert!(
        app.dialog.is_none(),
        "Delete must not act on the other selected file"
    );

    app.settings.pane_grid = Some([true, false]);
    let grid = run(&mut app, request(AxAction::Click));
    assert_file_parent(&grid);
    assert_eq!(grid.focus, folder_id);
    assert_eq!(app.selected(), vec![folder.clone(), file]);
    let row = grid.nodes.iter().find(|(id, _)| *id == folder_id).unwrap();
    assert_eq!(row.1.role(), Role::CheckBox);
    assert_eq!(row.1.toggled(), Some(Toggled::True));
    app.tab_mut().selected = BTreeSet::from([folder.clone()]);
    let spatial_focus = run(&mut app, key(egui::Key::ArrowRight));
    assert_eq!(spatial_focus.focus, file_id);
    run(&mut app, key(egui::Key::Delete));
    assert!(
        app.dialog.is_none(),
        "spatial focus cannot delete the old selection"
    );
    run(&mut app, request(AxAction::Focus));
    run(&mut app, key(egui::Key::F2));
    assert!(matches!(app.dialog, Some(Dialog::Rename { .. })));
    run(&mut app, vec![]);
    run(&mut app, key(egui::Key::Escape));
    let returned = run(&mut app, vec![]);
    assert_eq!(returned.focus, folder_id);
    assert_eq!(app.selected(), vec![folder.clone()]);
    run(&mut app, key(egui::Key::Enter));
    assert_eq!(app.tab().path, folder, "Enter opens the focused folder");
}

#[test]
fn accessibility_settings_migrate_and_round_trip() {
    let mut settings: Settings = serde_json::from_str("{}").unwrap();
    assert_eq!(settings.ui_scale, 100);
    assert!(!settings.high_contrast);
    let ctx = egui::Context::default();
    settings.set_ui_scale(&ctx, 999);
    assert_eq!(settings.ui_scale, 100);
    settings.set_ui_scale(&ctx, 150);
    settings.high_contrast = true;
    let mut storage = MemoryStorage::default();
    eframe::set_value(&mut storage, "settings", &settings);
    let restored: Settings = eframe::get_value(&storage, "settings").unwrap();
    assert_eq!(restored.ui_scale, 150);
    assert!(restored.high_contrast);
    let _ = ctx.run(egui::RawInput::default(), |_| {});
    assert_eq!(ctx.zoom_factor(), 1.5);
}
