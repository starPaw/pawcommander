use super::*;

#[test]
fn favorites_menu_blocks_alt_digit_and_releases_after_escape() {
    use egui::{Event, Key, Modifiers, PointerButton};

    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut frame = eframe::Frame::_new_kittest();
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let mut bookmark = Bookmark::new(target.clone().into());
    bookmark.shortcut = Some(1);
    app.bookmarks.push(bookmark);

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
    let output = run(&mut app, vec![]);
    let menu_label = crate::tr_format!("Действия для «target»", "Actions for target");
    assert!(output
        .platform_output
        .accesskit_update
        .as_ref()
        .unwrap()
        .nodes
        .iter()
        .any(|(_, node)| node.label() == Some(menu_label.as_str())));
    let menu_position = output
        .shapes
        .iter()
        .find_map(|clipped| match &clipped.shape {
            egui::Shape::Text(text) if text.galley.text() == "…" => {
                Some(text.pos + text.galley.size() * 0.5)
            }
            _ => None,
        })
        .expect("favorite menu button");
    for pressed in [true, false] {
        run(
            &mut app,
            vec![
                Event::PointerMoved(menu_position),
                Event::PointerButton {
                    pos: menu_position,
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: Modifiers::NONE,
                },
            ],
        );
    }
    run(&mut app, vec![]);
    let current = app.tab().path.clone();
    assert!(app.bookmark_menu_open.is_some());
    assert!(app.keyboard_blocked(&ctx));
    for pressed in [true, false] {
        run(
            &mut app,
            vec![Event::Key {
                key: Key::Num1,
                physical_key: Some(Key::Num1),
                pressed,
                repeat: false,
                modifiers: Modifiers::ALT,
            }],
        );
    }
    assert_eq!(app.tab().path, current, "Alt+1 stays inside the open menu");
    for pressed in [true, false] {
        run(
            &mut app,
            vec![Event::Key {
                key: Key::Escape,
                physical_key: Some(Key::Escape),
                pressed,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );
    }
    assert!(app.bookmark_menu_open.is_none());
    for pressed in [true, false] {
        run(
            &mut app,
            vec![Event::Key {
                key: Key::Num1,
                physical_key: Some(Key::Num1),
                pressed,
                repeat: false,
                modifiers: Modifiers::ALT,
            }],
        );
    }
    assert_eq!(app.tab().path, target.into());
}

#[test]
fn removed_favorite_does_not_leave_keyboard_blocked_by_menu_state() {
    let ctx = egui::Context::default();
    let root = tempfile::tempdir().unwrap();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let bookmark = Bookmark::new(root.path().join("favorite").into());
    app.bookmark_menu_open = Some(bookmark.id);
    app.bookmarks.push(bookmark);
    assert!(app.keyboard_blocked(&ctx));
    app.bookmarks.clear();
    assert!(!app.keyboard_blocked(&ctx));
}

#[test]
fn bookmark_store_migrates_legacy_and_round_trips_named_sftp() {
    let ctx = egui::Context::default();
    let root = tempfile::tempdir().unwrap();
    let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut storage = MemoryStorage::default();
    #[derive(Serialize)]
    struct Legacy {
        bookmarks: Vec<Location>,
    }
    let legacy_remote = fs_ops::resolve(
        "sftp://alice@example.com:2222/legacy",
        &Location::Local(PathBuf::from("/")),
    )
    .unwrap();
    eframe::set_value(
        &mut storage,
        PawCommander::SETTINGS_KEY,
        &Legacy {
            bookmarks: vec![
                PathBuf::from("/tmp/legacy-folder").into(),
                legacy_remote.clone(),
            ],
        },
    );
    cc.storage = Some(&storage);
    let legacy = PawCommander::new(&cc, Some(root.path().into()));
    assert_eq!(legacy.bookmarks.len(), 2);
    assert_eq!(legacy.bookmarks[0].name, "legacy-folder");
    assert_eq!(
        legacy.bookmarks[0].location,
        PathBuf::from("/tmp/legacy-folder").into()
    );
    assert_eq!(legacy.bookmarks[1].location, legacy_remote);
    drop(cc);

    let remote = fs_ops::resolve(
        "sftp://alice@example.com:2222/папка",
        &Location::Local(PathBuf::from("/")),
    )
    .unwrap();
    let mut bookmark = Bookmark::new(remote.clone());
    bookmark.name = "Remote work".into();
    bookmark.group = "Servers".into();
    bookmark.shortcut = Some(4);
    eframe::set_value(
        &mut storage,
        PawCommander::BOOKMARKS_KEY,
        &vec![SavedBookmark::from(&bookmark)],
    );
    let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
    cc.storage = Some(&storage);
    let restored = PawCommander::new(&cc, Some(root.path().into()));
    assert_eq!(restored.bookmarks.len(), 1);
    assert_eq!(restored.bookmarks[0].location, remote);
    assert_eq!(restored.bookmarks[0].name, "Remote work");
    assert_eq!(restored.bookmarks[0].group, "Servers");
    assert_eq!(restored.bookmarks[0].shortcut, Some(4));
}

#[test]
fn bookmark_shortcuts_reject_conflicts_and_stay_with_bookmark_on_reorder() {
    let mut bookmarks = vec![
        Bookmark::new(PathBuf::from("/a").into()),
        Bookmark::new(PathBuf::from("/b").into()),
        Bookmark::new(PathBuf::from("/c").into()),
    ];
    bookmarks[0].group = "Work".into();
    bookmarks[1].group = "Other".into();
    bookmarks[2].group = "Work".into();
    let edit_id = bookmarks[0].id;
    let chord = shortcuts::chord(egui::Key::Num1, egui::Modifiers::ALT);
    let shortcuts: Shortcuts = serde_json::from_value(serde_json::json!({
        "overrides": {"NewTab": chord}
    }))
    .expect("custom shortcut");
    assert!(!assign_bookmark_shortcut(
        &mut bookmarks,
        0,
        Some(1),
        &shortcuts
    ));
    assert!(assign_bookmark_shortcut(
        &mut bookmarks,
        0,
        Some(2),
        &Shortcuts::default()
    ));
    assert!(!assign_bookmark_shortcut(
        &mut bookmarks,
        1,
        Some(2),
        &Shortcuts::default()
    ));
    let peer = bookmark_peer(&bookmarks, 0, 1).expect("next bookmark in group");
    assert_eq!(peer, 2);
    assert!(move_bookmark(&mut bookmarks, 0, peer));
    assert_eq!(bookmarks[2].location, PathBuf::from("/a").into());
    assert_eq!(bookmarks[2].shortcut, Some(2));
    assert!(apply_bookmark_edit(
        &mut bookmarks,
        edit_id,
        "  Project files ",
        "  Projects  "
    ));
    assert_eq!(bookmarks[2].name, "Project files");
    assert_eq!(bookmarks[2].group, "Projects");
    let later_command: Shortcuts = serde_json::from_value(serde_json::json!({
        "overrides": {"NewTab": shortcuts::chord(egui::Key::Num2, egui::Modifiers::ALT)}
    }))
    .expect("later custom shortcut");
    assert!(validate_bookmark_shortcuts(&mut bookmarks, &later_command));
    assert!(bookmarks.iter().all(|bookmark| bookmark.shortcut.is_none()));
}
