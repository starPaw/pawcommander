use super::*;
use crate::test_ui::ContextExt as _;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant},
};

#[test]
fn file_comparison_dialog_shows_changes_and_passes_literal_paths_to_external_tool() {
    let temporary = tempfile::tempdir().unwrap();
    let folders = ["left", "right"].map(|name| {
        let path = temporary.path().join(name);
        fs::create_dir(&path).unwrap();
        path
    });
    let files = [
        folders[0].join("left ; $name\nwith spaces.txt"),
        folders[1].join("right 'quoted'.txt"),
    ];
    fs::write(&files[0], b"heading\nold entry\n").unwrap();
    fs::write(&files[1], b"heading\nnew entry\n").unwrap();
    let tool = temporary.path().join("diff fixture");
    fs::write(
        &tool,
        b"#!/bin/sh\nprintf '%s\\000' \"$#\" \"$@\" > \"$0.args\"\n",
    )
    .unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).unwrap();

    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(folders[0].clone()));
    app.settings.split = true;
    app.settings.diff_tool = tool.to_str().unwrap().to_owned();
    app.panes[1]
        .tab_mut()
        .navigate(folders[1].clone().into(), &ctx);
    for (pane, file) in app.panes.iter_mut().zip(&files) {
        let tab = pane.tab_mut();
        super::super::tests::wait_for_directory(tab);
        let entry = tab
            .entries
            .iter()
            .find(|entry| entry.path.path().file_name() == file.file_name())
            .unwrap()
            .path
            .clone();
        tab.select_revealed(entry);
    }
    let selected = app.files_to_compare().unwrap();
    app.run_shortcut(Command::CompareFiles, &ctx);
    assert!(app.comparison.is_some());

    let draw = |app: &mut PawCommander, events| {
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 680.0),
                )),
                events,
                ..Default::default()
            },
            |ui| app.show_comparison(ui.ctx()),
        )
        .shapes
        .into_iter()
        .filter_map(|clipped| match clipped.shape {
            egui::Shape::Text(label) => Some((
                label.galley.job.text.clone(),
                label.pos + label.galley.size() * 0.5,
            )),
            _ => None,
        })
        .collect::<Vec<_>>()
    };
    let wait = |app: &mut PawCommander, stage, wanted: &dyn Fn(&str) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut labels = Vec::new();
        while Instant::now() < deadline {
            eframe::App::logic(app, &ctx, &mut eframe::Frame::_new_kittest());
            labels = draw(app, vec![]);
            if labels.iter().any(|(label, _)| wanted(label)) {
                return labels;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("Expected content for {stage}. Rendered labels {labels:?}");
    };
    let labels = wait(&mut app, "text result", &|label| {
        label.contains("+ new entry")
    });
    assert!(labels
        .iter()
        .any(|(label, _)| label.contains("- old entry")));

    let click_external = |app: &mut PawCommander| {
        draw(app, vec![]);
        let label = text!("Открыть во внешнем инструменте");
        let pos = draw(app, vec![])
            .into_iter()
            .find(|(name, _)| name == label)
            .unwrap()
            .1;
        draw(app, vec![egui::Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            draw(
                app,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        }
    };
    click_external(&mut app);
    wait(&mut app, "tool handoff", &|label| {
        label == text!("Сравнение передано внешнему инструменту")
    });
    let received = fs::read(tool.with_extension("args")).unwrap();
    let mut expected = Vec::from(b"2\0".as_slice());
    for path in &selected {
        expected.extend_from_slice(path.path().as_os_str().as_encoded_bytes());
        expected.push(0);
    }
    assert_eq!(received, expected);
    fs::write(&tool, b"#!/bin/sh\nexit 19\n").unwrap();
    click_external(&mut app);
    wait(&mut app, "tool failure", &|label| {
        label.contains("exit status: 19")
    });
    assert_eq!(fs::read(&files[0]).unwrap(), b"heading\nold entry\n");
    assert_eq!(fs::read(&files[1]).unwrap(), b"heading\nnew entry\n");
}
