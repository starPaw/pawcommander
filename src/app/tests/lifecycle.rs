use super::*;

fn hidden_close_input() -> egui::RawInput {
    let mut input = egui::RawInput {
        focused: false,
        ..Default::default()
    };
    let viewport = input.viewports.get_mut(&egui::ViewportId::ROOT).unwrap();
    viewport.minimized = Some(true);
    viewport.occluded = Some(true);
    viewport.events.push(egui::ViewportEvent::Close);
    input
}

fn hidden_close_tick(app: &mut PawCommander, ctx: &egui::Context) -> bool {
    let output = ctx.run_logic(&hidden_close_input(), |ctx| {
        eframe::App::logic(app, ctx, &mut eframe::Frame::_new_kittest());
    });
    output
        .viewport_commands
        .get(&egui::ViewportId::ROOT)
        .is_some_and(|commands| commands.contains(&egui::ViewportCommand::CancelClose))
}

#[test]
fn hidden_close_waits_for_active_and_queued_jobs_without_ui() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let (started, observed) = mpsc::channel();
    let (release_first, first_wait) = mpsc::channel();
    let first_started = started.clone();
    app.start_progress_job(&ctx, "first", move |_| {
        first_started.send(0).unwrap();
        first_wait
            .recv_timeout(Duration::from_secs(5))
            .map_err(|error| error.to_string())?;
        Ok("first done".into())
    });
    assert_eq!(observed.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
    let (release_second, second_wait) = mpsc::channel();
    app.start_progress_job(&ctx, "second", move |_| {
        started.send(1).unwrap();
        second_wait
            .recv_timeout(Duration::from_secs(5))
            .map_err(|error| error.to_string())?;
        Ok("second done".into())
    });
    assert_eq!(app.pending_jobs.len(), 1);
    assert!(hidden_close_tick(&mut app, &ctx));
    assert_eq!(app.job.as_ref().unwrap().label, "first");
    assert_eq!(
        app.notice.as_ref().unwrap().0,
        crate::i18n::text("Дождитесь завершения файловой операции, затем закройте окно.")
    );

    release_first.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.job.as_ref().is_none_or(|job| job.label != "second") {
        assert!(hidden_close_tick(&mut app, &ctx));
        assert!(Instant::now() < deadline, "hidden queue did not advance");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(observed.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
    assert!(app.pending_jobs.is_empty());
    assert!(hidden_close_tick(&mut app, &ctx));

    release_second.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let canceled = hidden_close_tick(&mut app, &ctx);
        if app.job.is_none() {
            assert!(!canceled, "completed hidden operations must allow closing");
            break;
        }
        assert!(canceled);
        assert!(Instant::now() < deadline, "hidden result was not consumed");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn hidden_close_protects_a_paused_queue_but_allows_an_idle_app() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    assert!(!hidden_close_tick(&mut app, &ctx));

    app.queue_start_failed = true;
    app.start_progress_job(&ctx, "pending", |_| Ok("done".into()));
    assert!(app.job.is_none());
    assert_eq!(app.pending_jobs.len(), 1);
    assert!(hidden_close_tick(&mut app, &ctx));
    assert_eq!(app.pending_jobs.len(), 1);

    app.pending_jobs.clear();
    assert!(!hidden_close_tick(&mut app, &ctx));
}
