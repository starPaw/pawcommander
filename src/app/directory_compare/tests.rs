use super::*;
use crate::test_ui::ContextExt as _;
use std::{fs, time::UNIX_EPOCH};

#[test]
fn directory_comparison_dialog_runs_both_modes_and_displays_differences() {
    let temporary = tempfile::tempdir().unwrap();
    let paths = ["left", "right"].map(|name| {
        let path = temporary.path().join(name);
        fs::create_dir(&path).unwrap();
        path
    });
    for (folder, bytes) in paths.iter().zip([b"abc", b"xyz"]) {
        let file = folder.join("equal-size.bin");
        fs::write(&file, bytes).unwrap();
        fs::File::open(&file)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_690_000_000)),
            )
            .unwrap();
    }
    fs::write(paths[0].join("left.txt"), b"left").unwrap();
    fs::write(paths[1].join("right.txt"), b"right").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(paths[0].clone()));
    app.panes[1]
        .tab_mut()
        .navigate(paths[1].clone().into(), &ctx);
    app.settings.split = false;
    app.run_shortcut(Command::CompareFolders, &ctx);
    assert!(app.comparison.is_none());
    app.settings.split = true;
    app.run_shortcut(Command::CompareFolders, &ctx);
    assert!(app.keyboard_blocked(&ctx));

    let draw = |app: &mut PawCommander, events| {
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(940.0, 640.0),
                )),
                events,
                ..Default::default()
            },
            |ui| app.show_directory_comparison(ui.ctx()),
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
    let click = |app: &mut PawCommander, label: &'static str| {
        draw(app, vec![]);
        let pos = draw(app, vec![])
            .into_iter()
            .find(|(value, _)| value == text(label))
            .unwrap_or_else(|| panic!("Missing button {label}"))
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
    let wait = |app: &mut PawCommander| {
        for _ in 0..200 {
            eframe::App::logic(app, &ctx, &mut eframe::Frame::_new_kittest());
            if matches!(
                app.comparison.as_ref().map(|comparison| &comparison.phase),
                Some(Phase::Finished(_))
            ) {
                draw(app, vec![]);
                return draw(app, vec![]);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("Comparison did not finish");
    };
    click(&mut app, "Сравнить");
    let labels = wait(&mut app);
    for filename in ["left.txt", "right.txt"] {
        assert!(labels.iter().any(|(label, _)| label == filename));
    }
    assert!(!labels.iter().any(|(label, _)| label == "equal-size.bin"));

    click(&mut app, "По содержимому");
    click(&mut app, "Сравнить");
    let labels = wait(&mut app);
    assert!(labels.iter().any(|(label, _)| label == "equal-size.bin"));
    assert!(labels.iter().any(|(label, _)| label == text("Различаются")));
    click(&mut app, "Закрыть");
    assert!(app.comparison.is_none());
    assert_eq!(fs::read(paths[0].join("equal-size.bin")).unwrap(), b"abc");
    assert_eq!(fs::read(paths[1].join("equal-size.bin")).unwrap(), b"xyz");
}
