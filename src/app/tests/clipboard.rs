use super::*;

#[test]
fn unrevisioned_file_clipboard_uses_observed_contents() {
    let file_list = FileList {
        paths: vec![PathBuf::from("first.txt")],
        cut: true,
        revision: None,
    };
    let mut different = file_list.clone();
    different.paths.push(PathBuf::from("second.txt"));
    let clipboard = Clipboard::new(Vec::new(), true, ClipboardOrigin::System(file_list.clone()));
    assert!(clipboard.matches(Some(&file_list)));
    assert!(!clipboard.matches(Some(&different)));
    assert!(!clipboard.matches(None));
}

#[cfg(target_os = "macos")]
mod macos_input {
    use super::*;
    use crate::app::clipboard::{file_clipboard_action, translate_macos_clipboard_events};

    fn key(key: egui::Key, pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn file_paste_captures_press_mode_and_ignores_all_release_orders() {
        for move_here in [false, true] {
            let modifiers = egui::Modifiers {
                alt: move_here,
                ..egui::Modifiers::COMMAND
            };
            for release_modifiers in [
                modifiers,
                egui::Modifiers::COMMAND,
                egui::Modifiers::ALT,
                egui::Modifiers::NONE,
            ] {
                let mut events = vec![
                    key(egui::Key::V, true, modifiers),
                    egui::Event::ModifiersChanged(release_modifiers),
                    key(egui::Key::V, false, release_modifiers),
                ];
                translate_macos_clipboard_events(&mut events, || None);
                let actions: Vec<_> = events.iter().filter_map(file_clipboard_action).collect();
                assert!(matches!(&actions[..], [Action::Paste(mode)] if *mode == move_here));
            }
        }
    }

    #[test]
    fn file_paste_uses_egui_repeat_detection_and_focus_reset() {
        let ctx = egui::Context::default();
        for key_code in [egui::Key::V, egui::Key::Paste] {
            for (events, expected) in [
                (vec![key(key_code, true, egui::Modifiers::COMMAND)], 1),
                // winit repeat flags are false before egui sees the key.
                (vec![key(key_code, true, egui::Modifiers::COMMAND)], 0),
                (vec![egui::Event::WindowFocused(false)], 0),
                (
                    vec![
                        egui::Event::WindowFocused(true),
                        key(key_code, true, egui::Modifiers::COMMAND),
                    ],
                    1,
                ),
                (vec![key(key_code, false, egui::Modifiers::NONE)], 0),
            ] {
                let mut events = events;
                translate_macos_clipboard_events(&mut events, || None);
                let _ = ctx.run_test_ui(
                    egui::RawInput {
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        let count = ui.ctx().input(|input| {
                            input
                                .events
                                .iter()
                                .filter_map(file_clipboard_action)
                                .count()
                        });
                        assert_eq!(count, expected);
                    },
                );
            }
        }
    }

    #[test]
    fn unfocused_raw_paste_does_not_start_or_replay_a_file_job() {
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.txt");
        let target = directory.path().join("destination");
        std::fs::write(&source, "clipboard payload").unwrap();
        std::fs::create_dir(&target).unwrap();
        let mut app = PawCommander::new(&cc, Some(target.clone()));
        app.clipboard = Some(Clipboard::new(
            vec![source.clone().into()],
            false,
            ClipboardOrigin::App(None),
        ));
        for focused in [false, true] {
            let mut input = egui::RawInput {
                focused,
                events: if focused {
                    vec![]
                } else {
                    vec![key(egui::Key::V, true, egui::Modifiers::COMMAND)]
                },
                ..Default::default()
            };
            eframe::App::raw_input_hook(&mut app, &ctx, &mut input);
            let _ = ctx.run_test_ui(input, |ui| app.keyboard(ui.ctx()));
            assert!(app.job.is_none());
            assert!(app.pending_jobs.is_empty());
            assert!(app.notice.is_none());
            assert!(!app.conflict_open());
            assert_eq!(
                app.clipboard.as_ref().unwrap().paths,
                vec![source.clone().into()]
            );
        }
        assert!(!target.join("source.txt").exists());
        assert_eq!(
            std::fs::read_to_string(source).unwrap(),
            "clipboard payload"
        );
    }

    #[test]
    fn clipboard_translation_keeps_text_commands_and_native_paste_events() {
        let mut events = vec![
            key(egui::Key::C, true, egui::Modifiers::COMMAND),
            key(egui::Key::X, true, egui::Modifiers::COMMAND),
            key(egui::Key::Copy, true, egui::Modifiers::NONE),
            key(egui::Key::Cut, true, egui::Modifiers::NONE),
            key(egui::Key::V, true, egui::Modifiers::COMMAND),
            key(egui::Key::Paste, true, egui::Modifiers::NONE),
            egui::Event::Paste("native request".into()),
        ];
        translate_macos_clipboard_events(&mut events, || Some("two\r\nwords".into()));
        assert!(matches!(
            &events[..],
            [egui::Event::Copy, egui::Event::Cut, egui::Event::Copy, egui::Event::Cut,
                egui::Event::Paste(first), egui::Event::Key { key: egui::Key::V, .. },
                egui::Event::Paste(second), egui::Event::Key { key: egui::Key::Paste, .. },
                egui::Event::Paste(native)]
                if first == "two\nwords" && second == first && native == "native request"
        ));
        for modifiers in [
            egui::Modifiers::NONE,
            egui::Modifiers::CTRL,
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            egui::Modifiers::COMMAND | egui::Modifiers::ALT,
        ] {
            let mut events: Vec<_> = [egui::Key::C, egui::Key::X, egui::Key::V]
                .into_iter()
                .map(|code| key(code, true, modifiers))
                .collect();
            let original = events.clone();
            translate_macos_clipboard_events(&mut events, || panic!("not a text paste"));
            assert_eq!(events, original);
        }
    }

    #[test]
    fn translated_paste_edits_text_in_root_and_immediate_viewport_paths() {
        for immediate in [false, true] {
            let ctx = egui::Context::default();
            let cc = eframe::CreationContext::_new_kittest(ctx.clone());
            let directory = tempfile::tempdir().unwrap();
            let mut app = PawCommander::new(&cc, Some(directory.path().into()));
            let source = directory.path().join("clipboard source.txt");
            std::fs::write(&source, "unchanged").unwrap();
            app.clipboard = Some(Clipboard::new(
                vec![source.clone().into()],
                false,
                ClipboardOrigin::App(None),
            ));
            let id = egui::Id::unique("clipboard-text");
            let mut text = String::new();
            let mut input = egui::RawInput::default();
            if immediate {
                input.viewport_id = egui::ViewportId::from_hash_of("quick-look");
                input
                    .viewports
                    .insert(input.viewport_id, Default::default());
            }
            let _ = ctx.run_test_ui(input.clone(), |ui| {
                ui.add(egui::TextEdit::singleline(&mut text).id(id))
                    .request_focus();
            });
            let mut events = vec![key(egui::Key::V, true, egui::Modifiers::COMMAND)];
            if !immediate {
                translate_macos_clipboard_events(&mut events, || Some("Привет text".into()));
            }
            input.events = events;
            let _ = ctx.run_test_ui(input, |ui| {
                if immediate {
                    assert_ne!(ui.ctx().viewport_id(), egui::ViewportId::ROOT);
                    ui.ctx().input_mut(|input| {
                        translate_macos_clipboard_events(&mut input.events, || {
                            Some("Привет text".into())
                        });
                    });
                }
                // Root dispatch runs before the focused TextEdit renders.
                app.keyboard(ui.ctx());
                ui.add(egui::TextEdit::singleline(&mut text).id(id));
            });
            assert_eq!(text, "Привет text");
            assert!(app.job.is_none());
            assert!(app.pending_jobs.is_empty());
            assert!(app.notice.is_none());
            assert!(!app.conflict_open());
            assert_eq!(
                app.clipboard.as_ref().unwrap().paths,
                vec![source.clone().into()]
            );
            assert_eq!(std::fs::read_to_string(source).unwrap(), "unchanged");
        }
    }
}
