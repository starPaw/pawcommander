use super::*;

#[test]
fn recovery_worker_tracks_active_operation_and_clears_known_completion() {
    for fail in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("recovery");
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        app.recovery = crate::recovery::Recovery::for_test(directory.clone(), &ctx);
        app.history = History::for_test(root.path().join("history.json"), &ctx);
        let (started, observed) = mpsc::channel();
        let (release, held) = mpsc::channel();
        app.start_progress_job(&ctx, "tracked job", move |progress| {
            let source = Location::Local(PathBuf::from("tracked source"));
            progress.results.plan([(source.clone(), None)]);
            progress.results.begin(&source);
            started.send(()).unwrap();
            held.recv().unwrap();
            if fail {
                Err("known operation failure".into())
            } else {
                progress.results.record(
                    &source,
                    None,
                    crate::result_history::Outcome::Completed,
                    None,
                );
                Ok("done".into())
            }
        });
        observed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        assert!(crate::recovery::scan(&directory).unfinished.is_empty());
        release.send(()).unwrap();
        poll_queue_until(&mut app, &ctx, |app| app.job.is_none());
        assert!(crate::recovery::scan(&directory).unfinished.is_empty());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        assert_eq!(app.notice.as_ref().unwrap().1, fail);
    }
}

#[test]
fn recovery_worker_preserves_marker_after_caught_panic() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("recovery");
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.recovery = crate::recovery::Recovery::for_test(directory.clone(), &ctx);
    app.start_progress_job(&ctx, "panicked job", |_| panic!("injected worker panic"));
    poll_queue_until(&mut app, &ctx, |app| app.job.is_none());
    let report = crate::recovery::scan(&directory);
    assert!(report.errors.is_empty());
    assert_eq!(report.unfinished.len(), 1);
    assert_eq!(report.unfinished[0].label, "panicked job");
    assert!(app.notice.as_ref().unwrap().1);
}

#[test]
fn recovery_worker_refuses_to_start_without_persisted_marker() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("blocked-recovery");
    std::fs::write(&directory, b"foreign file").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.recovery = crate::recovery::Recovery::for_test(directory.clone(), &ctx);
    let changed = root.path().join("must-not-exist");
    let worker_changed = changed.clone();
    app.start_progress_job(&ctx, "blocked job", move |_| {
        std::fs::write(worker_changed, b"unexpected work").unwrap();
        Ok("done".into())
    });
    poll_queue_until(&mut app, &ctx, |app| app.job.is_none());
    assert!(!changed.exists());
    assert_eq!(std::fs::read(directory).unwrap(), b"foreign file");
    assert!(app.notice.as_ref().unwrap().1);
}

#[test]
fn recovery_worker_retains_marker_when_history_cannot_be_saved() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("recovery");
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.recovery = crate::recovery::Recovery::for_test(directory.clone(), &ctx);
    // The default test history has no disk worker and cannot acknowledge persistence.
    app.start_progress_job(&ctx, "history failure", |progress| {
        progress
            .results
            .plan([(Location::Local(PathBuf::from("source")), None)]);
        Ok("finished operation".into())
    });
    poll_queue_until(&mut app, &ctx, |app| app.job.is_none());
    assert_eq!(crate::recovery::scan(&directory).unfinished.len(), 1);
    assert!(app.notice.as_ref().unwrap().1);
}

#[test]
fn recovery_window_blocks_file_shortcuts_and_drops() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.recovery.reopen();
    assert!(app.keyboard_blocked(&ctx));
    assert!(!app.drop_enabled());
}

#[test]
fn recovery_report_suspends_existing_new_folder_dialog_enter() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.dialog = Some(Dialog::NewFolder {
        parent: root.path().to_path_buf().into(),
        name: "must-not-be-created".into(),
    });
    app.recovery.reopen();
    let mut frame = eframe::Frame::_new_kittest();
    let _ = ctx.run_test_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(1000.0, 700.0),
            )),
            events: vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        },
        |ui| eframe::App::ui(&mut app, ui, &mut frame),
    );
    assert!(app.recovery.is_open());
    assert!(matches!(app.dialog, Some(Dialog::NewFolder { .. })));
    assert!(app.job.is_none());
    assert!(!root.path().join("must-not-be-created").exists());
}
