use super::*;

#[test]
fn properties_shortcut_opens_selected_group_without_navigating() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("folder")).unwrap();
    std::fs::write(root.path().join("file"), b"data").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    wait_for_directory(app.tab_mut());
    app.tab_mut().selected = BTreeSet::from([
        root.path().join("folder").into(),
        root.path().join("file").into(),
    ]);
    let before = app.tab().path.clone();
    let _ = ctx.run(
        egui::RawInput {
            modifiers: egui::Modifiers::ALT,
            events: vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::ALT,
            }],
            ..Default::default()
        },
        |ctx| app.keyboard(ctx),
    );
    assert!(app.properties.is_some());
    assert_eq!(app.tab().path, before);
    assert_eq!(app.selected().len(), 2);
}

#[test]
fn pane_shortcuts_leave_text_and_modal_input_alone() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let focus = egui::Id::new("test_filter");
    let key = |key, modifiers| egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    };
    let run = |app: &mut PawCommander, events, focused| {
        let _ = ctx.run(
            egui::RawInput {
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let response =
                        ui.add(egui::TextEdit::singleline(&mut app.tab_mut().filter).id(focus));
                    if focused {
                        response.request_focus();
                    } else {
                        response.surrender_focus();
                    }
                });
                app.keyboard(ctx);
            },
        );
    };
    run(
        &mut app,
        vec![key(egui::Key::F3, egui::Modifiers::NONE)],
        true,
    );
    assert!(
        app.settings.split,
        "a focused text field blocks pane commands"
    );
    run(
        &mut app,
        vec![key(egui::Key::L, egui::Modifiers::COMMAND)],
        true,
    );
    assert!(
        app.tab().edit_address,
        "the original text navigation chord still works"
    );

    app.shortcut_editor = Some(ShortcutEditor::new(&app.settings.shortcuts));
    run(
        &mut app,
        vec![key(egui::Key::F3, egui::Modifiers::NONE)],
        false,
    );
    assert!(
        app.settings.split,
        "the shortcut editor blocks pane commands"
    );
    app.shortcut_editor = None;
    run(
        &mut app,
        vec![key(egui::Key::F3, egui::Modifiers::SHIFT)],
        false,
    );
    assert!(app.settings.split, "extra modifiers do not match a binding");
    run(
        &mut app,
        vec![
            egui::Event::Key {
                key: egui::Key::F3,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            key(egui::Key::F3, egui::Modifiers::NONE),
        ],
        false,
    );
    assert!(!app.settings.split);
}

#[test]
fn custom_toolbar_shortcut_does_not_consume_text_editing_keys() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let enter = egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::Enter);
    app.settings.shortcuts = serde_json::from_value(serde_json::json!({
        "overrides": {"Open": null, "FocusToolbar": enter}
    }))
    .unwrap();
    let _ = ctx.run(
        egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        },
        |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.text_edit_singleline(&mut app.tab_mut().address)
                    .request_focus();
            });
            app.keyboard(ctx);
            assert!(ctx.input(|input| input.key_pressed(egui::Key::Enter)));
        },
    );
    assert!(!app.focus_toolbar);
}

#[test]
fn command_palette_filters_navigates_runs_and_restores_focus() {
    use egui::{Event, Key, Modifiers};

    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut frame = eframe::Frame::_new_kittest();
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.action(Action::NewTab(0), &ctx);
    app.settings.shortcuts = serde_json::from_value(serde_json::json!({
        "overrides": {"NewTab": shortcuts::chord(Key::F9, Modifiers::NONE)}
    }))
    .unwrap();
    app.tab_mut().focus_filter = true;

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
    };
    let key = |key, pressed, modifiers| Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers,
    };
    let open = || {
        let modifiers = Modifiers::COMMAND | Modifiers::SHIFT;
        vec![key(Key::P, true, modifiers), key(Key::P, false, modifiers)]
    };

    run(&mut app, vec![]);
    let previous = ctx.memory(|memory| memory.focused()).unwrap();
    run(&mut app, open());
    assert!(app.command_palette.is_some());
    run(&mut app, vec![]);
    run(&mut app, vec![Event::Text("not-a-command".into())]);
    run(&mut app, vec![key(Key::Escape, true, Modifiers::NONE)]);
    assert!(app.command_palette.is_none());
    assert!(ctx.memory(|memory| memory.has_focus(previous)));
    assert!(app.tab().filter.is_empty());
    run(
        &mut app,
        vec![
            key(Key::Escape, true, Modifiers::NONE),
            key(Key::Escape, false, Modifiers::NONE),
        ],
    );
    assert!(ctx.memory(|memory| memory.has_focus(previous)));

    run(&mut app, open());
    run(&mut app, vec![]);
    let output = run(&mut app, vec![Event::Text("TAB".into())]);
    let tree = output.platform_output.accesskit_update.unwrap();
    assert!(tree.nodes.iter().any(|(_, node)| {
        node.label() == Some(Command::NewTab.label()) && node.keyboard_shortcut() == Some("F9")
    }));
    assert!(!tree.nodes.iter().any(|(_, node)| {
        node.label() == Some(Command::NewFolder.label()) && node.keyboard_shortcut().is_some()
    }));
    run(&mut app, vec![key(Key::ArrowDown, true, Modifiers::NONE)]);
    run(&mut app, vec![key(Key::Enter, true, Modifiers::NONE)]);
    assert!(app.command_palette.is_none());
    assert_eq!(app.panes[0].tabs.len(), 1);
}

#[test]
fn pane_tab_keeps_event_order_without_traversing_widget_focus() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let key = |key, modifiers, pressed| egui::Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers,
    };
    let tab = |pressed| key(egui::Key::Tab, egui::Modifiers::NONE, pressed);
    let run = |app: &mut PawCommander, events| {
        let mut input = egui::RawInput {
            events,
            ..Default::default()
        };
        eframe::App::raw_input_hook(app, &ctx, &mut input);
        let _ = ctx.run(input, |ctx| {
            app.keyboard(ctx);
            egui::CentralPanel::default().show(ctx, |ui| {
                let _ = ui.button("Widget focus target");
            });
        });
    };
    run(
        &mut app,
        vec![tab(true), key(egui::Key::T, egui::Modifiers::COMMAND, true)],
    );
    assert_eq!(app.active, 1);
    assert_eq!(app.panes[1].tabs.len(), 2, "Tab dispatched before New tab");
    assert!(!ctx.wants_keyboard_input());
    run(&mut app, vec![tab(true)]);
    assert_eq!(app.active, 1, "held Tab does not switch panes again");
    run(&mut app, vec![tab(false), tab(true)]);
    assert_eq!(app.active, 0);

    app.settings.shortcuts = serde_json::from_value(serde_json::json!({
        "overrides": { "OtherPane": null }
    }))
    .unwrap();
    run(&mut app, vec![tab(false), tab(true)]);
    assert_eq!(app.active, 0);
    assert!(ctx.wants_keyboard_input(), "unbound Tab traverses widgets");
}

#[test]
fn starting_filter_input_blocks_a_later_file_shortcut_in_the_same_frame() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("keep.txt");
    std::fs::write(&path, b"data").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    wait_for_directory(app.tab_mut());
    app.tab_mut().selected.insert(path.into());
    assert_eq!(app.selected().len(), 1);

    let key = |key, modifiers| egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    };
    let _ = ctx.run(
        egui::RawInput {
            events: vec![
                key(egui::Key::F, egui::Modifiers::COMMAND),
                key(egui::Key::Delete, egui::Modifiers::NONE),
            ],
            ..Default::default()
        },
        |ctx| app.keyboard(ctx),
    );
    assert!(app.tab().focus_filter);
    assert!(app.dialog.is_none());
}
