use super::*;
use crate::tools::custom_action::Template;
use std::fs;
use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};

#[test]
fn custom_actions_preview_and_launch_the_same_literal_selection() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("work space");
    fs::create_dir(&folder).unwrap();
    let files = [
        folder.join("a 'Café' file.txt"),
        folder.join("z\n$ literal.txt"),
    ];
    for path in &files {
        fs::write(path, b"fixture").unwrap();
    }
    let record = root.path().join("received args");
    let program = root.path().join("record arguments");
    fs::write(
        &program,
        b"#!/bin/sh\noutput=$1\nshift\nprintf '%s\\000' \"$(pwd -P)\" \"$@\" >> \"$output\"\n",
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let definition = Template {
        name: "Record selected items".into(),
        program: program.to_str().unwrap().into(),
        arguments: vec![record.to_str().unwrap().into(), "".into(), "{files}".into()],
    };

    let ctx = egui::Context::default();
    let mut storage = MemoryStorage::default();
    eframe::Storage::set_string(
        &mut storage,
        PawCommander::SETTINGS_KEY,
        "(preview:false,split:false)".into(),
    );
    let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
    cc.storage = Some(&storage);
    let mut app = PawCommander::new(&cc, Some(folder.clone()));
    assert!(
        app.settings.custom_actions.is_empty(),
        "old profiles default to no actions"
    );
    app.settings.custom_actions.push(definition.clone());
    eframe::App::save(&mut app, &mut storage);

    let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
    cc.storage = Some(&storage);
    let mut app = PawCommander::new(&cc, Some(folder.clone()));
    assert_eq!(app.settings.custom_actions, [definition]);
    wait_for_directory(app.tab_mut());
    app.tab_mut()
        .selected
        .extend(files.iter().cloned().map(Location::from));

    let draw = |app: &mut PawCommander, events| {
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 720.0),
                )),
                focused: true,
                events,
                ..Default::default()
            },
            |ui| app.dialogs(ui.ctx()),
        )
    };
    app.run_shortcut(Command::CustomActions, &ctx);
    assert!(matches!(app.dialog, Some(Dialog::RunCustomAction(_))));
    draw(&mut app, Vec::new());
    draw(
        &mut app,
        vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }],
    );
    assert!(app.dialog.is_none() && app.application_job.is_none() && !record.exists());

    app.run_shortcut(Command::CustomActions, &ctx);
    draw(&mut app, Vec::new());
    let output = draw(&mut app, Vec::new());
    let texts: Vec<_> = output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text),
            _ => None,
        })
        .collect();
    for (index, file) in files.iter().enumerate() {
        let expected = format!("{}  {:?}", index + 3, file.as_os_str());
        assert!(texts
            .iter()
            .any(|text| text.galley.text().contains(&expected)));
    }
    let run = texts
        .iter()
        .find(|text| matches!(text.galley.text(), "Run" | "Запустить"))
        .unwrap();
    let point = run.pos + run.galley.size() * 0.5;
    // Change the live selection after the launch preview has captured it.
    app.tab_mut().selected.clear();
    assert!(!record.exists());
    for pressed in [true, false] {
        draw(
            &mut app,
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }
    assert!(app.dialog.is_none() && app.application_job.is_some());
    poll_queue_until(&mut app, &ctx, |app| app.application_job.is_none());
    assert!(app.notice.as_ref().is_some_and(|(_, error)| !error));
    let bytes = fs::read(record).unwrap();
    let cwd = folder.canonicalize().unwrap();
    let expected: [&[u8]; 5] = [
        cwd.as_os_str().as_bytes(),
        b"",
        files[0].as_os_str().as_bytes(),
        files[1].as_os_str().as_bytes(),
        b"",
    ];
    assert_eq!(bytes.split(|byte| *byte == 0).collect::<Vec<_>>(), expected);
}
