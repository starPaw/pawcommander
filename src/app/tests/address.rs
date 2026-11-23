use super::*;

#[cfg(target_os = "macos")]
#[test]
fn terminal_command_uses_the_current_folder_and_reports_a_launch_error() {
    let fixture = tempfile::tempdir().unwrap();
    let file = fixture.path().join("no longer a folder.txt");
    std::fs::write(&file, b"fixture").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(fixture.path().into()));
    wait_for_directory(app.tab_mut());
    app.tab_mut().path = Location::Local(file.clone());
    app.run_shortcut(Command::OpenTerminal, &ctx);
    assert!(app.application_job.is_some());
    app.active = 1 - app.active;
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.application_job.is_some() && Instant::now() < deadline {
        app.poll(&ctx);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(app.application_job.is_none());
    let (notice, failed) = app.notice.unwrap();
    assert!(
        failed && notice.contains(file.to_str().unwrap()),
        "{notice}"
    );
    assert!(notice.contains(crate::i18n::text!("Выберите папку для открытия терминала.")));
    assert_eq!(std::fs::read(file).unwrap(), b"fixture");
}

#[cfg(target_os = "macos")]
#[test]
fn terminal_application_job_reports_its_origin_if_the_worker_stops() {
    use crate::app::actions::ApplicationJob;

    let folder = Location::Local(PathBuf::from("/fixture/original terminal folder"));
    let (sender, result) = std::sync::mpsc::channel();
    let job = ApplicationJob::Terminal { folder, result };
    assert!(job.poll_notice().is_none());
    drop(sender);
    let (message, error) = job.poll_notice().unwrap();
    assert!(error && message.contains("/fixture/original terminal folder"));
    assert!(message.contains(crate::i18n::text!("Запуск приложения прервался.")));
}

#[test]
fn address_editor_submits_typed_paths_completes_folders_and_cancels() {
    let ctx = egui::Context::default();
    Theme::Frappe.apply(&ctx, false);
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("Two Paths")).unwrap();
    std::fs::create_dir_all(root.path().join("Two Words/Nested folder")).unwrap();
    std::fs::write(root.path().join("Two Before Folders.txt"), "").unwrap();
    let original: Location = root.path().to_path_buf().into();
    let mut tab = Tab::new(original.clone(), &ctx);
    tab.begin_address();
    let mut time = 0.0;
    let suggestion_position = std::cell::Cell::new(None);
    let mut render = |tab: &mut Tab, events| {
        time += 0.1;
        let mut actions = vec![];
        let output = ctx.run_test_ui(
            egui::RawInput {
                time: Some(time),
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(440.0, 200.0),
                )),
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    address_bar(ui, tab, 1, &mut actions, &mut DropTargets::default());
                });
            },
        );
        suggestion_position.set(output.shapes.iter().find_map(|shape| {
            if let egui::Shape::Text(text) = &shape.shape
                && text.galley.text() == "Nested folder"
            {
                return Some(text.visual_bounding_rect().center());
            }
            None
        }));
        actions
    };
    let key = |key| egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    render(&mut tab, vec![]);
    render(&mut tab, vec![egui::Event::Paste("two words".into())]);
    assert_eq!(
        tab.address, "two words",
        "editing starts with the full path selected"
    );
    let actions = render(&mut tab, vec![key(egui::Key::Enter)]);
    assert!(actions
        .iter()
        .any(|a| matches!(a, Action::Navigate(1, p) if *p == original.join("two words"))));
    assert!(!tab.edit_address);

    let wait_for_suggestions = |tab: &mut Tab, render: &mut dyn FnMut(&mut Tab, Vec<_>) -> _| {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !egui::Popup::is_any_open(&ctx) {
            assert!(Instant::now() < deadline, "expected folder suggestions");
            std::thread::sleep(Duration::from_millis(5));
            render(tab, vec![]);
        }
    };
    tab.begin_address();
    render(&mut tab, vec![]);
    render(&mut tab, vec![egui::Event::Paste("two".into())]);
    wait_for_suggestions(&mut tab, &mut render);
    render(&mut tab, vec![key(egui::Key::ArrowDown)]);
    render(&mut tab, vec![key(egui::Key::ArrowDown)]);
    ctx.memory_mut(|memory| {
        let field = memory.focused().expect("path field focus");
        memory.set_focus_lock_filter(field, egui::EventFilter::default());
    });
    let actions = render(&mut tab, vec![key(egui::Key::Tab)]);
    assert!(tab.edit_address);
    assert_eq!(
        tab.address,
        format!(
            "{}{}",
            original.join("Two Words").display(),
            std::path::MAIN_SEPARATOR
        )
    );
    assert!(!actions.iter().any(|a| matches!(a, Action::Navigate(_, _))));
    render(&mut tab, vec![egui::Event::Text("nest".into())]);
    assert!(
        tab.address.ends_with("nest"),
        "Tab keeps the path field focused"
    );
    wait_for_suggestions(&mut tab, &mut render);
    render(&mut tab, vec![key(egui::Key::ArrowDown)]);
    let actions = render(&mut tab, vec![key(egui::Key::Enter)]);
    assert!(actions.iter().any(|a| matches!(
        a, Action::Navigate(1, p) if *p == original.join("Two Words").join("Nested folder")
    )));
    assert!(!tab.edit_address);

    tab.begin_address();
    render(&mut tab, vec![]);
    render(&mut tab, vec![egui::Event::Paste("Two Words/nest".into())]);
    wait_for_suggestions(&mut tab, &mut render);
    // Run another frame so the popup has completed egui's initial sizing pass.
    render(&mut tab, vec![]);
    let pos = suggestion_position.get().expect("visible suggestion");
    let pointer = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    render(
        &mut tab,
        vec![egui::Event::PointerMoved(pos), pointer(true)],
    );
    let actions = render(&mut tab, vec![pointer(false)]);
    assert!(actions.iter().any(|a| matches!(
        a, Action::Navigate(1, p) if *p == original.join("Two Words").join("Nested folder")
    )));
    assert!(!tab.edit_address);

    tab.begin_address();
    render(&mut tab, vec![]);
    render(
        &mut tab,
        vec![egui::Event::Paste("discard this edit".into())],
    );
    let actions = render(&mut tab, vec![key(egui::Key::Escape)]);
    assert!(!tab.edit_address);
    assert_eq!(tab.address, original.display());
    assert!(!actions.iter().any(|a| matches!(a, Action::Navigate(_, _))));
}

#[test]
fn relative_address_preserves_spaces() {
    assert_eq!(
        crate::address::Completion::default()
            .resolve("two words", &PathBuf::from("/tmp").into())
            .unwrap(),
        Location::Local(PathBuf::from("/tmp/two words"))
    );
}
