use super::*;

#[cfg(target_os = "macos")]
#[test]
fn file_job_delivers_the_original_io_error_to_the_ui() {
    use crate::app::file_result::FileResult;
    use crate::file_error::{Failure, LocalPathAction, MutationError};
    use crate::operations::TransferError;

    let fixture = tempfile::tempdir().unwrap();
    let parent = Location::Local(fixture.path().canonicalize().unwrap());
    let target = parent.join("existing");
    std::fs::create_dir(target.path()).unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(parent.path().to_owned()));
    app.start_file_job(&ctx, "New folder", target.display(), move |_| {
        FileResult::Action {
            result: crate::vfs::create_directory(&parent, "existing").map(|path| path.display()),
            target: Some(parent.join("existing")),
            stopped: None,
        }
    });

    let completion = app
        .jobs
        .current
        .as_ref()
        .unwrap()
        .result
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    let JobCompletion::File {
        result,
        history_warning: None,
    } = completion
    else {
        panic!("expected a typed file completion");
    };
    let FileResult::Action {
        result: Err(TransferError::Failed(Failure::Mutation(error))),
        target: Some(requested),
        ..
    } = *result
    else {
        panic!("expected a typed folder error");
    };
    assert_eq!(requested, target);
    assert!(matches!(
        error.as_ref(),
        MutationError::LocalIo { path, action: LocalPathAction::CreateDirectory, source }
            if path == target.path() && source.raw_os_error() == Some(libc::EEXIST)
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn removing_queued_template_creation_cancels_its_typed_reply_without_creating_a_file() {
    use crate::operations::TransferError;
    use crate::vfs::file_templates::{NewFile, Source, TextEncoding};

    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let _held = hold_queue(&mut app);
    let (completion, result) = mpsc::channel();
    app.enqueue_new_file(
        NewFile {
            directory: Location::Local(root.path().to_owned()),
            name: "untouched.txt".into(),
            source: Source::Text {
                value: "draft".into(),
                encoding: TextEncoding::default(),
            },
        },
        completion,
        &ctx,
    );
    assert_eq!(app.jobs.pending.len(), 1);

    app.remove_pending_job(0);

    assert!(app.jobs.pending.is_empty());
    assert!(matches!(
        result.try_recv().unwrap().as_ref(),
        Err(TransferError::Cancelled)
    ));
    assert!(!root.path().join("untouched.txt").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn editor_launch_error_and_remote_save_result_remain_visible_in_the_same_poll() {
    use crate::app::actions::ApplicationJob;
    use crate::tools::launch::{CommandDetails, InvocationError, Target};

    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("working copy.txt");
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(fixture.path().into()));
    let (report_launch, result) = mpsc::channel();
    app.application_job = Some(ApplicationJob::Configured {
        details: CommandDetails {
            program: fixture.path().join("editor"),
            folder: fixture.path().to_owned(),
            target: Target::Editor {
                source: Location::Local(path.clone()),
                file: path,
            },
        },
        result,
    });
    let report_save = hold_queue(&mut app);
    app.jobs.current.as_mut().unwrap().remote_save = true;
    report_launch
        .send(Err(InvocationError::WorkerStart(std::io::Error::other(
            "editor launch failed",
        ))))
        .unwrap();
    report_save
        .send(JobCompletion::Text(Err("save preparation refused".into())))
        .unwrap();

    app.poll(&ctx);

    assert!(app.application_job.is_none() && app.jobs.current.is_none());
    let (notice, error) = app.notice.unwrap();
    assert!(
        error
            && notice.contains("editor launch failed")
            && notice.contains("save preparation refused"),
        "{notice}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn file_promise_completion_preserves_worker_failure_and_queue_cancellation() {
    use crate::{
        file_error::{Failure, FilePromiseError},
        native_drag::{PromiseKind, PromiseRequest},
        operations::TransferError,
    };

    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().to_owned()));
    let source: crate::vfs::Location = root.path().join("local source").into();
    let target = root.path().join("receiver file");
    let enqueue = |app: &mut PawCommander| {
        let (completion, result) = mpsc::channel();
        app.enqueue_file_promise(
            PromiseRequest {
                source: source.clone(),
                kind: PromiseKind::File,
                target: target.clone(),
                completion,
            },
            &ctx,
        );
        result
    };

    app.jobs.start_failed = true;
    assert!(matches!(enqueue(&mut app).try_recv().unwrap(),
        Err(TransferError::Failed(Failure::FilePromise(reason)))
            if matches!(reason.as_ref(), FilePromiseError::QueueUnavailable)));
    app.jobs.start_failed = false;

    let held = hold_queue(&mut app);
    let queued = enqueue(&mut app);
    assert!(matches!(queued.try_recv(), Err(mpsc::TryRecvError::Empty)));
    app.remove_pending_job(0);
    assert!(matches!(
        queued.try_recv(),
        Ok(Err(TransferError::Cancelled))
    ));
    held.send(JobCompletion::Text(Ok("release".into())))
        .unwrap();
    poll_queue_until(&mut app, &ctx, |app| !app.jobs.is_busy());

    let error = enqueue(&mut app)
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(error, TransferError::Failed(Failure::FilePromise(reason))
        if matches!(reason.as_ref(), FilePromiseError::RemoteSourceRequired(path) if path == &source))
    );
    poll_queue_until(&mut app, &ctx, |app| !app.jobs.is_busy());
    assert!(!target.exists());
}

#[test]
fn checksum_commands_save_a_manifest_and_keep_distinct_results_in_history() {
    use std::fs;

    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("files");
    fs::create_dir(&folder).unwrap();
    let changed = folder.join("changed.txt");
    let missing = folder.join("missing.txt");
    fs::write(&changed, b"abc").unwrap();
    fs::write(&missing, b"abc").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(folder.clone()));
    let history_path = root.path().join("history.json");
    app.history = History::for_test(history_path.clone(), &ctx);
    wait_for_directory(app.tab_mut());
    app.tab_mut()
        .selected
        .extend([changed.clone().into(), missing.clone().into()]);

    let palette_frame = |app: &mut PawCommander, events, keys: &[egui::Key]| {
        ctx.run_test_ui(
            egui::RawInput {
                events,
                ..Default::default()
            },
            |ui| app.show_command_palette(ui.ctx(), keys),
        );
    };
    let choose = |app: &mut PawCommander, command: Command| {
        app.run_shortcut(Command::Palette, &ctx);
        assert!(app.command_palette.is_some());
        palette_frame(app, vec![], &[]);
        palette_frame(app, vec![egui::Event::Text(command.label().into())], &[]);
        palette_frame(app, vec![], &[egui::Key::Enter]);
        assert!(app.command_palette.is_none());
    };
    choose(&mut app, Command::CreateChecksumManifest);
    assert!(matches!(&app.dialog, Some(Dialog::CreateChecksumManifest {
        sources, name, ..
    }) if sources.len() == 2 && name == "SHA256SUMS"));
    ctx.run_test_ui(egui::RawInput::default(), |ui| app.dialogs(ui.ctx()));
    for pressed in [true, false] {
        ctx.run_test_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| app.dialogs(ui.ctx()),
        );
    }
    assert!(app.dialog.is_none() && app.jobs.current.is_some());
    poll_queue_until(&mut app, &ctx, |app| app.jobs.current.is_none());
    wait_for_directory(app.tab_mut());
    let manifest = folder.join("SHA256SUMS");
    assert!(manifest.is_file());
    fs::write(changed, b"xyz").unwrap();
    fs::remove_file(missing).unwrap();
    let listed = app
        .tab()
        .entries
        .iter()
        .find(|entry| entry.name == "SHA256SUMS")
        .expect("newly created manifest is listed")
        .path
        .clone();
    app.tab_mut().select_revealed(listed.clone());
    assert_eq!(app.selected(), [listed]);
    choose(&mut app, Command::VerifyChecksumManifest);
    assert!(
        app.jobs.current.is_some(),
        "notice={:?}, address_open={}",
        app.notice,
        app.tab().edit_address
    );
    poll_queue_until(&mut app, &ctx, |app| app.jobs.current.is_none());
    assert!(app.notice.as_ref().is_some_and(|(_, error)| *error));
    let history: serde_json::Value =
        serde_json::from_slice(&fs::read(history_path).unwrap()).unwrap();
    let [created, operation] = history["records"].as_array().unwrap().as_slice() else {
        panic!("Expected creation and verification records, got {history}");
    };
    assert_eq!(created["label"], Command::CreateChecksumManifest.label());
    assert_eq!(created["selected_items_only"], true);
    assert_eq!(operation["label"], Command::VerifyChecksumManifest.label());
    assert_eq!(operation["selected_items_only"], true);
    for (name, reason) in [
        (
            "changed.txt",
            crate::i18n::text!("Файл изменён (SHA-256 не совпала)"),
        ),
        ("missing.txt", crate::i18n::text!("Файл отсутствует")),
    ] {
        assert!(
            operation["items"].as_array().unwrap().iter().any(|item| {
                item["source"].as_str().unwrap().ends_with(name) && item["reason"] == reason
            }),
            "{operation:?}"
        );
    }
}

#[test]
fn history_retry_owns_one_queued_attempt_and_keeps_completed_files_untouched() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let target = root.path().join("target");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(source.join("a"), "retry me").unwrap();
    std::fs::write(source.join("b"), "already completed").unwrap();
    std::fs::write(target.join("a"), "existing target").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(source.clone()));
    let history_path = root.path().join("history.json");
    app.history = crate::result_history::History::for_test(history_path.clone(), &ctx);
    app.settings.verify_copies = true;
    app.start_transfer(
        &ctx,
        "original",
        vec![source.join("a").into(), source.join("b").into()],
        target.clone().into(),
        TransferKind::Copy,
        false,
    );
    poll_queue_until(&mut app, &ctx, PawCommander::conflict_open);
    answer_queue_conflict(&mut app, &ctx, crate::i18n::text!("Пропустить"));
    poll_queue_until(&mut app, &ctx, |app| app.jobs.current.is_none());
    assert_eq!(app.jobs.retries.len(), 1);
    let id = app.jobs.retries[0].id.clone();
    assert!(app.history.retryable.contains(&id));
    std::fs::write(source.join("b"), "new source content").unwrap();
    app.settings.verify_copies = false;
    let held = hold_queue(&mut app);
    app.retry_transfer(&ctx, &id);
    app.retry_transfer(&ctx, &id);
    assert_eq!(
        app.jobs.pending.len(),
        1,
        "a repeated click cannot enqueue twice"
    );
    assert!(
        app.jobs.pending[0].description.contains("SHA-256"),
        "retry retains the original verification setting"
    );
    assert!(!app.history.retryable.contains(&id));
    assert!(app.jobs.retries.is_empty());
    app.remove_pending_job(0);
    assert!(
        app.history.retryable.contains(&id),
        "removing an unstarted retry restores its action"
    );
    app.retry_transfer(&ctx, &id);
    assert_eq!(app.jobs.pending.len(), 1);
    held.send(JobCompletion::Text(Ok("release".into())))
        .unwrap();
    poll_queue_until(&mut app, &ctx, PawCommander::conflict_open);
    answer_queue_conflict(&mut app, &ctx, crate::i18n::text!("Сохранить обе версии"));
    poll_queue_until(&mut app, &ctx, |app| app.jobs.current.is_none());
    assert!(app.jobs.retries.is_empty());
    assert_eq!(std::fs::read(target.join("a")).unwrap(), b"existing target");
    assert_eq!(std::fs::read(target.join("a (2)")).unwrap(), b"retry me");
    assert_eq!(
        std::fs::read(target.join("b")).unwrap(),
        b"already completed"
    );
    assert_eq!(
        std::fs::read(source.join("b")).unwrap(),
        b"new source content"
    );
    assert!(!target.join("b (2)").exists());
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(history_path).unwrap()).unwrap();
    let records = saved["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["id"], id);
    assert_ne!(records[1]["id"], records[0]["id"]);
    assert_eq!(records[1]["items"].as_array().unwrap().len(), 1);
    let restarted = PawCommander::new(&cc, Some(source));
    assert!(
        restarted.jobs.retries.is_empty(),
        "saved display paths are not retry authority after restart"
    );
}

#[test]
fn copy_verification_defaults_off_persists_and_is_captured_per_queued_job() {
    let mut settings: Settings = serde_json::from_str("{}").unwrap();
    assert!(!settings.verify_copies);
    settings.verify_copies = true;
    let restored: Settings =
        serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
    assert!(restored.verify_copies);
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("destination");
    std::fs::create_dir(&destination).unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let _held = hold_queue(&mut app);
    for (name, verify, kind) in [
        ("verified", true, TransferKind::Copy),
        ("ordinary", false, TransferKind::Copy),
        ("moved", true, TransferKind::Move),
    ] {
        let source = root.path().join(name);
        std::fs::write(&source, name).unwrap();
        app.settings.verify_copies = verify;
        app.start_transfer(
            &ctx,
            name,
            vec![source.into()],
            destination.clone().into(),
            kind,
            false,
        );
    }
    app.settings.verify_copies = true;
    for (name, verify) in [("verified", true), ("ordinary", false), ("moved", false)] {
        let job = app.jobs.pending.pop_front().unwrap();
        assert_eq!(job.description.contains("SHA-256"), verify);
        let mut progress = Progress::default();
        job.run_text_work(&mut progress).unwrap();
        assert_eq!(
            progress.copy_verification(),
            verify,
            "queued job owns its setting"
        );
        assert_eq!(
            std::fs::read(destination.join(name)).unwrap(),
            name.as_bytes()
        );
    }
    assert!(!root.path().join("moved").exists());
}

#[test]
fn copy_verification_footer_distinguishes_reads_from_writes() {
    let ctx = egui::Context::default();
    let progress = crate::operations::Snapshot {
        item: Some(PathBuf::from("/disposable/source/file")),
        verification: Some(crate::operations::Verification {
            read: 1024,
            expected: 2048,
        }),
        total_written: 2048,
        ..Default::default()
    };
    let mut output = None;
    for _ in 0..2 {
        output = Some(ctx.run_test_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| operation_details(ui, &progress, false));
        }));
    }
    let text = operation_footer_text(&output.unwrap());
    assert!(
        text.contains(crate::i18n::text!("Проверка SHA-256")),
        "{text}"
    );
    assert!(text.contains("1.0 KB / 2.0 KB"), "{text}");
    assert!(text.contains("2.0 KB"), "{text}");
    assert!(
        !text.contains("3.0 KB"),
        "read bytes must not inflate writes: {text}"
    );
}

#[test]
fn queue_serializes_workers_reorders_removes_and_continues_after_panic_without_ui() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let (order, observed) = mpsc::channel();
    let first_order = order.clone();
    let (release, blocked) = mpsc::channel();
    app.start_progress_job(&ctx, "first", move |_| {
        first_order.send(0).unwrap();
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok("first done".into())
    });
    assert_eq!(observed.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
    for index in 1..=3 {
        let order = order.clone();
        app.start_progress_job(&ctx, &format!("queued {index}"), move |_| {
            order.send(index).unwrap();
            if index == 2 {
                panic!("worker panic must release queue ownership");
            }
            Ok(format!("done {index}"))
        });
    }
    assert_eq!(app.jobs.pending.len(), 3);
    assert!(
        observed.try_recv().is_err(),
        "pending workers must not start"
    );
    app.move_pending_job(1, -1);
    app.move_pending_job(0, -1); // Out-of-range controls cannot reorder the active job.
    app.jobs.pending.remove(2);
    assert_eq!(app.jobs.pending[0].label, "queued 2");
    release.send(()).unwrap();
    poll_queue_until(&mut app, &ctx, |app| !app.jobs.is_busy());
    assert_eq!(observed.try_iter().collect::<Vec<_>>(), vec![2, 1]);
    assert_eq!(app.notice, Some(("done 1".into(), false)));
}

#[test]
fn queue_is_bounded_and_removing_pending_work_has_no_side_effects() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let held = hold_queue(&mut app);
    let (started, observed) = mpsc::channel();
    for _ in 0..=MAX_PENDING_JOBS {
        let started = started.clone();
        app.start_progress_job(&ctx, "pending", move |_| {
            started.send(()).unwrap();
            Ok("done".into())
        });
    }
    assert_eq!(app.jobs.pending.len(), MAX_PENDING_JOBS);
    assert!(app.notice.as_ref().unwrap().0.contains("100"));
    assert!(app.notice.as_ref().unwrap().1);
    app.jobs.pending.clear();
    held.send(JobCompletion::Text(Ok("done".into()))).unwrap();
    app.poll(&ctx);
    assert!(app.jobs.current.is_none());
    assert!(observed.try_recv().is_err());
}

#[test]
fn queue_spawn_failure_preserves_work_and_order_until_explicit_retry() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let held = hold_queue(&mut app);
    let (started, observed) = mpsc::channel();
    for index in 0..2 {
        let started = started.clone();
        let destination = root.path().join(format!("job-{index}"));
        app.start_progress_job(&ctx, &format!("job {index}"), move |_| {
            std::fs::write(destination, b"captured job payload")
                .map_err(|error| error.to_string())?;
            started.send(index).unwrap();
            Ok(format!("done {index}"))
        });
    }
    app.jobs.fail_spawn = true;
    held.send(JobCompletion::Text(Ok("released".into())))
        .unwrap();
    app.poll(&ctx);
    assert!(app.jobs.current.is_none());
    assert!(app.jobs.start_failed);
    assert_eq!(app.jobs.pending.len(), 2);
    assert_eq!(app.jobs.pending[0].label, "job 0");
    assert_eq!(app.jobs.pending[1].label, "job 1");
    assert!(observed.try_recv().is_err());
    assert!(!root.path().join("job-0").exists());
    app.jobs.fail_spawn = false;
    app.poll(&ctx);
    assert!(
        app.jobs.current.is_none(),
        "OS recovery does not cause unbounded automatic retries"
    );
    operation_footer_frame(&mut app, &ctx, vec![]);
    let output = operation_footer_frame(&mut app, &ctx, vec![]);
    let retry = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text)
                if text.galley.text() == crate::i18n::text!("Повторить запуск очереди") =>
            {
                Some(text.pos + text.galley.size() * 0.5)
            }
            _ => None,
        })
        .expect("failed queue exposes explicit retry");
    for pressed in [true, false] {
        operation_footer_frame(
            &mut app,
            &ctx,
            vec![
                egui::Event::PointerMoved(retry),
                egui::Event::PointerButton {
                    pos: retry,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }
    poll_queue_until(&mut app, &ctx, |app| !app.jobs.is_busy());
    assert_eq!(observed.try_iter().collect::<Vec<_>>(), vec![0, 1]);
    for index in 0..2 {
        assert_eq!(
            std::fs::read(root.path().join(format!("job-{index}"))).unwrap(),
            b"captured job payload"
        );
    }
}

#[test]
fn queued_same_target_copies_revalidate_conflicts_capture_destination_and_save_ordered_history() {
    for (decision, keep_both) in [
        (crate::i18n::text!("Заменить"), false),
        (crate::i18n::text!("Сохранить обе версии"), true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        let target = root.path().join("target");
        let other = root.path().join("other");
        for path in [&first, &second, &target, &other] {
            std::fs::create_dir(path).unwrap();
        }
        std::fs::write(first.join("same.txt"), b"first payload").unwrap();
        std::fs::write(second.join("same.txt"), b"second payload").unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        let history_path = root.path().join("history.json");
        app.history = History::for_test(history_path.clone(), &ctx);
        let held = hold_queue(&mut app);
        for (label, source) in [("first", &first), ("second", &second)] {
            app.start_transfer(
                &ctx,
                label,
                vec![source.join("same.txt").into()],
                target.clone().into(),
                TransferKind::Copy,
                false,
            );
        }
        assert!(
            app.jobs.transfer().is_none(),
            "a pending transfer cannot take active interaction ownership"
        );
        assert!(!target.join("same.txt").exists());
        app.action(Action::Navigate(0, other.clone().into()), &ctx);
        held.send(JobCompletion::Text(Ok("released".into())))
            .unwrap();
        poll_queue_until(&mut app, &ctx, PawCommander::conflict_open);
        assert_eq!(app.jobs.current.as_ref().unwrap().label, "second");
        assert_eq!(
            std::fs::read(target.join("same.txt")).unwrap(),
            b"first payload"
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&history_path).unwrap()).unwrap();
        assert_eq!(
            saved["records"].as_array().unwrap().len(),
            1,
            "first result persisted before second conflict"
        );
        assert_eq!(saved["records"][0]["label"], "first");
        answer_queue_conflict(&mut app, &ctx, decision);
        poll_queue_until(&mut app, &ctx, |app| app.jobs.current.is_none());
        assert_eq!(
            std::fs::read(target.join("same.txt")).unwrap(),
            if keep_both {
                b"first payload".as_slice()
            } else {
                b"second payload".as_slice()
            }
        );
        if keep_both {
            assert_eq!(
                std::fs::read(target.join("same (2).txt")).unwrap(),
                b"second payload"
            );
        }
        assert_eq!(
            std::fs::read_dir(&other).unwrap().count(),
            0,
            "navigation cannot retarget queued copies"
        );
        assert_eq!(
            std::fs::read(first.join("same.txt")).unwrap(),
            b"first payload"
        );
        assert_eq!(
            std::fs::read(second.join("same.txt")).unwrap(),
            b"second payload"
        );
        assert_eq!(
            std::fs::read_dir(&target).unwrap().count(),
            if keep_both { 2 } else { 1 },
            "no staging remains"
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&history_path).unwrap()).unwrap();
        assert_eq!(saved["records"].as_array().unwrap().len(), 2);
        assert_eq!(saved["records"][1]["label"], "second");
        assert_eq!(saved["records"][1]["items"][0]["outcome"], "Completed");
    }
}

#[test]
fn queued_transfer_failure_and_conflict_stop_release_only_the_active_job() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let target = root.path().join("target");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(source.join("conflict.txt"), b"new").unwrap();
    std::fs::write(target.join("conflict.txt"), b"old").unwrap();
    std::fs::write(source.join("next.txt"), b"next").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let held = hold_queue(&mut app);
    for name in ["missing.txt", "conflict.txt", "next.txt"] {
        app.start_transfer(
            &ctx,
            name,
            vec![source.join(name).into()],
            target.clone().into(),
            TransferKind::Copy,
            false,
        );
    }
    held.send(JobCompletion::Text(Ok("released".into())))
        .unwrap();
    poll_queue_until(&mut app, &ctx, PawCommander::conflict_open);
    assert_eq!(app.jobs.current.as_ref().unwrap().label, "conflict.txt");
    assert_eq!(app.jobs.pending.len(), 1);
    assert!(!target.join("next.txt").exists());
    app.request_transfer_stop(&ctx);
    assert_eq!(
        app.jobs.current.as_ref().unwrap().label,
        "conflict.txt",
        "Stop waits for terminal handoff"
    );
    poll_queue_until(&mut app, &ctx, |app| !app.jobs.is_busy());
    assert_eq!(std::fs::read(target.join("conflict.txt")).unwrap(), b"old");
    assert_eq!(std::fs::read(source.join("conflict.txt")).unwrap(), b"new");
    assert_eq!(std::fs::read(target.join("next.txt")).unwrap(), b"next");
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 2);
}

#[test]
fn queued_copy_waits_for_destination_creation_and_source_rename() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.txt");
    let target = root.path().join("created later");
    std::fs::write(&source, b"source payload").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let held = hold_queue(&mut app);
    let parent: Location = root.path().to_path_buf().into();
    app.start_progress_job(&ctx, "create destination", move |_| {
        fs_ops::create_directory(&parent, "created later")
            .map(|path| path.display())
            .map_err(|error| crate::i18n::file_error(&error))
    });
    app.start_transfer(
        &ctx,
        "copy after create",
        vec![source.clone().into()],
        target.clone().into(),
        TransferKind::Copy,
        false,
    );
    let rename_source: Location = source.clone().into();
    app.start_progress_job(&ctx, "rename source", move |_| {
        fs_ops::rename_entry(&rename_source, "renamed.txt")
            .map(|path| path.display())
            .map_err(|error| crate::i18n::file_error(&error))
    });
    app.start_transfer(
        &ctx,
        "stale source",
        vec![source.clone().into()],
        target.clone().into(),
        TransferKind::Copy,
        false,
    );
    assert!(
        !target.exists(),
        "queued operations must not mutate the destination"
    );
    held.send(JobCompletion::Text(Ok("released".into())))
        .unwrap();
    poll_queue_until(&mut app, &ctx, |app| !app.jobs.is_busy());
    assert_eq!(
        std::fs::read(target.join("source.txt")).unwrap(),
        b"source payload"
    );
    assert_eq!(
        std::fs::read(root.path().join("renamed.txt")).unwrap(),
        b"source payload"
    );
    assert!(!source.exists());
    assert!(
        app.notice.as_ref().unwrap().1,
        "stale source failure must remain visible"
    );
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 1);
}

#[test]
fn queued_cut_moves_reserve_sources_and_do_not_consume_replaced_clipboard() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("cut.txt");
    let target = root.path().join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(&source, b"cut payload").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.clipboard = Some(Clipboard::new(
        vec![source.clone().into()],
        true,
        ClipboardOrigin::App(None),
    ));
    let held = hold_queue(&mut app);
    for _ in 0..2 {
        app.start_transfer(
            &ctx,
            "cut move",
            vec![source.clone().into()],
            target.clone().into(),
            TransferKind::Move,
            true,
        );
    }
    assert_eq!(app.jobs.pending.len(), 1, "duplicate cut move is rejected");
    assert!(source.exists());
    let replacement: Location = root.path().join("new clipboard.txt").into();
    app.clipboard = Some(Clipboard::new(
        vec![replacement.clone()],
        true,
        ClipboardOrigin::App(None),
    ));
    held.send(JobCompletion::Text(Ok("released".into())))
        .unwrap();
    poll_queue_until(&mut app, &ctx, |app| !app.jobs.is_busy());
    assert!(!source.exists());
    assert_eq!(
        std::fs::read(target.join("cut.txt")).unwrap(),
        b"cut payload"
    );
    assert_eq!(app.clipboard.as_ref().unwrap().paths, vec![replacement]);
    assert!(app.jobs.current.is_none());
}

#[test]
fn result_history_blocks_file_shortcuts_while_reading_selectable_labels() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("keep.txt");
    std::fs::write(&file, b"unchanged").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    wait_for_directory(app.tab_mut());
    app.tab_mut().selected.insert(file.clone().into());
    app.show_history = true;
    assert!(app.keyboard_blocked(&ctx));
    assert!(!app.drop_enabled());
    let events = [
        (egui::Key::C, egui::Modifiers::COMMAND),
        (egui::Key::X, egui::Modifiers::COMMAND),
        (egui::Key::Delete, egui::Modifiers::NONE),
    ]
    .into_iter()
    .flat_map(|(key, modifiers)| {
        [true, false].map(move |pressed| egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers,
        })
    })
    .collect();
    let mut input = egui::RawInput {
        events,
        ..Default::default()
    };
    eframe::App::raw_input_hook(&mut app, &ctx, &mut input);
    let _ = ctx.run_test_ui(input, |ui| {
        let ctx = &ui.ctx().clone();
        app.keyboard(ctx);
        app.history.show(ctx, &mut app.show_history);
    });
    assert!(
        app.clipboard.is_none(),
        "history selection must not copy/cut underlying file paths"
    );
    assert!(
        app.dialog.is_none(),
        "Delete must not open a file deletion dialog"
    );
    assert!(app.notice.is_none());
    assert_eq!(std::fs::read(&file).unwrap(), b"unchanged");
    assert!(app.tab().selected.contains(&file.into()));
}

#[test]
fn transfer_notice_shows_remaining_paths_and_bounds_long_lists() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let path = "/disposable/source/remaining-tree".to_owned();
    app.notice = Some((
        crate::i18n::transfer_finished(0, 0, std::slice::from_ref(&path), false, true, "/target"),
        false,
    ));
    operation_footer_frame(&mut app, &ctx, vec![]);
    let output = operation_footer_frame(&mut app, &ctx, vec![]);
    let label = output
        .shapes
        .iter()
        .find_map(|clipped| match &clipped.shape {
            egui::Shape::Text(text) if text.galley.text().contains(&path) => {
                Some((clipped.clip_rect, text))
            }
            _ => None,
        })
        .expect("terminal notice includes the full remaining path");
    for row in &label.1.galley.rows {
        let rect = row.rect().translate(label.1.pos.to_vec2());
        assert!(
            label.0.contains_rect(rect),
            "notice row {rect:?} clipped by {:?}",
            label.0
        );
    }

    let paths = (0..100)
        .map(|index| format!("/disposable/source/remaining-{index:03}"))
        .collect::<Vec<_>>();
    app.notice = Some((
        crate::i18n::transfer_finished(1, 2, &paths, false, true, "/target"),
        false,
    ));
    operation_footer_frame(&mut app, &ctx, vec![]);
    let output = operation_footer_frame(&mut app, &ctx, vec![]);
    let clip = output
        .shapes
        .iter()
        .find_map(|clipped| match &clipped.shape {
            egui::Shape::Text(text) if text.galley.text().contains("remaining-099") => {
                Some(clipped.clip_rect)
            }
            _ => None,
        })
        .unwrap();
    assert!(
        clip.height() <= 166.0,
        "long notices must preserve room for navigation: {clip:?}"
    );
    assert!(
        clip.top() > 350.0,
        "footer remains bounded at a 600px window height"
    );
    let mut scrolled = output;
    for _ in 0..10 {
        scrolled = operation_footer_frame(
            &mut app,
            &ctx,
            vec![
                egui::Event::PointerMoved(egui::pos2(100.0, clip.center().y)),
                egui::Event::MouseWheel {
                    phase: egui::TouchPhase::Move,
                    source: egui::MouseWheelSource::Unknown,
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -10_000.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }
    let (clip, text) = scrolled
        .shapes
        .iter()
        .find_map(|clipped| match &clipped.shape {
            egui::Shape::Text(text) if text.galley.text().contains("remaining-099") => {
                Some((clipped.clip_rect, text))
            }
            _ => None,
        })
        .unwrap();
    let last = text
        .galley
        .rows
        .last()
        .unwrap()
        .rect()
        .translate(text.pos.to_vec2());
    assert!(
        clip.contains_rect(last),
        "last remaining path must become visible after scrolling: {last:?} clipped by {clip:?}"
    );
}

#[test]
fn queue_active_notice_stays_bounded_and_keeps_stop_visible() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let _held = hold_queue(&mut app);
    let (_events, events) = mpsc::channel();
    app.jobs.current.as_mut().unwrap().transfer = Some(TransferInteraction {
        events,
        conflict: None,
        can_pause: false,
    });
    app.notice = Some((
        (0..1000)
            .map(|index| format!("previous remaining path {index}\n"))
            .collect(),
        true,
    ));
    operation_footer_frame(&mut app, &ctx, vec![]);
    let output = operation_footer_frame(&mut app, &ctx, vec![]);
    let notice_clip = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text)
                if text.galley.text().contains("previous remaining path 999") =>
            {
                Some(shape.clip_rect)
            }
            _ => None,
        })
        .unwrap();
    assert!(
        notice_clip.height() <= 86.0,
        "previous result must scroll within a bounded area: {notice_clip:?}"
    );
    let (clip, stop) = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "Stop" => Some((
                shape.clip_rect,
                egui::Rect::from_min_size(text.pos, text.galley.size()),
            )),
            _ => None,
        })
        .expect("Stop remains rendered");
    assert!(clip.contains_rect(stop), "Stop must remain visible");
    assert!(
        stop.top() > 350.0,
        "footer must leave room for file navigation"
    );
}

#[test]
fn transfer_stop_button_keeps_pending_ownership_and_navigation_until_result() {
    let root = tempfile::tempdir().unwrap();
    let other = root.path().join("another folder");
    std::fs::create_dir(&other).unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let (result, receiver) = mpsc::channel();
    let (reporter, progress) = Progress::tracked();
    app.jobs.current = Some(Job {
        transfer: None,
        cut: None,
        label: "test operation".into(),
        started: Instant::now(),
        result: receiver,
        undo: None,
        progress,
        #[cfg(target_os = "macos")]
        remote_save: false,
        #[cfg(target_os = "macos")]
        export_target: None,
    });
    let ordinary = operation_footer_frame(&mut app, &ctx, vec![]);
    assert!(!operation_footer_text(&ordinary).contains("Stop"));
    app.request_transfer_stop(&ctx);
    assert!(
        !reporter.cancellation().is_requested(),
        "generic jobs have no Stop"
    );

    let (events, receiver) = mpsc::channel();
    app.jobs.current.as_mut().unwrap().transfer = Some(TransferInteraction {
        events: receiver,
        conflict: None,
        can_pause: false,
    });
    // A disconnected events channel precedes terminal-result publication.
    drop(events);
    app.poll(&ctx);
    assert!(app.jobs.transfer().is_some());
    let focus = ctx.memory(|memory| memory.focused());
    operation_footer_frame(&mut app, &ctx, vec![]);
    assert_eq!(
        ctx.memory(|memory| memory.focused()),
        focus,
        "Stop must not take initial keyboard focus"
    );
    let output = operation_footer_frame(&mut app, &ctx, vec![]);
    let stop = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "Stop" => {
                Some(text.pos + text.galley.size() * 0.5)
            }
            _ => None,
        })
        .expect("active transfer exposes Stop");
    for pressed in [true, false] {
        operation_footer_frame(
            &mut app,
            &ctx,
            vec![
                egui::Event::PointerMoved(stop),
                egui::Event::PointerButton {
                    pos: stop,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }
    assert!(reporter.cancellation().is_requested());
    let pending = operation_footer_frame(&mut app, &ctx, vec![]);
    assert!(operation_footer_text(&pending).contains("Stopping…"));
    app.request_transfer_stop(&ctx);
    app.action(Action::Navigate(0, other.clone().into()), &ctx);
    assert_eq!(app.tab().path.path(), other);
    app.poll(&ctx);
    assert!(app.jobs.transfer().is_some());
    assert!(
        app.drop_enabled(),
        "new work may queue while the active transfer stops"
    );

    // Errors after Stop preserve recovery guidance rather than becoming success.
    result
        .send(JobCompletion::Text(Err(
            "cleanup failed: /retained/private-staging".into(),
        )))
        .unwrap();
    app.poll(&ctx);
    assert!(app.jobs.current.is_none());
    assert_eq!(
        app.notice,
        Some(("cleanup failed: /retained/private-staging".into(), true))
    );
}

#[test]
fn transfer_pause_controls_resume_or_stop_the_active_job_before_the_queue_advances() {
    use crate::operations::PauseState;
    use std::fs;

    for stop in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("file.txt");
        let destination = root.path().join("destination");
        let target = destination.join("file.txt");
        fs::create_dir(&destination).unwrap();
        fs::write(&source, b"new content").unwrap();
        fs::write(&target, b"old content").unwrap();

        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        app.start_transfer(
            &ctx,
            "copy to pause",
            vec![source.clone().into()],
            destination.into(),
            TransferKind::Copy,
            false,
        );
        poll_queue_until(&mut app, &ctx, PawCommander::conflict_open);
        let (reached, boundary) = mpsc::channel();
        let (release, checkpoint) = mpsc::channel();
        let mut checkpoint = Some(checkpoint);
        app.jobs
            .current
            .as_ref()
            .unwrap()
            .progress
            .cancel
            .set_checkpoint(move || {
                if let Some(checkpoint) = checkpoint.take() {
                    reached.send(()).unwrap();
                    checkpoint.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                false
            });
        answer_queue_conflict(&mut app, &ctx, crate::i18n::text!("Заменить"));
        boundary.recv_timeout(Duration::from_secs(5)).unwrap();

        let pause_state = |app: &PawCommander| {
            app.jobs
                .current
                .as_ref()
                .and_then(|job| job.progress.pause_state())
        };
        let click = |app: &mut PawCommander, label| {
            operation_footer_frame(app, &ctx, vec![]);
            let output = operation_footer_frame(app, &ctx, vec![]);
            let pos = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == label => {
                        Some(text.pos + text.galley.size() * 0.5)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("Missing {label} in {}", operation_footer_text(&output)));
            for pressed in [true, false] {
                operation_footer_frame(
                    app,
                    &ctx,
                    vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        };
        assert_eq!(pause_state(&app), Some(PauseState::Running));
        click(&mut app, "Pause");
        assert_eq!(pause_state(&app), Some(PauseState::Requested));
        let waiting = operation_footer_frame(&mut app, &ctx, vec![]);
        assert!(operation_footer_text(&waiting).contains("Pausing transfer…"));
        release.send(()).unwrap();
        poll_queue_until(&mut app, &ctx, |app| {
            pause_state(app) == Some(PauseState::Paused)
        });
        let paused = operation_footer_frame(&mut app, &ctx, vec![]);
        assert!(operation_footer_text(&paused).contains("Transfer paused"));

        let queued_target = target.clone();
        let (started, observed) = mpsc::channel();
        app.start_progress_job(&ctx, "queued observer", move |_| {
            started.send(fs::read(queued_target).unwrap()).unwrap();
            Ok("observed".into())
        });
        app.poll(&ctx);
        assert!(matches!(
            observed.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert_eq!(app.jobs.pending.len(), 1);
        click(&mut app, if stop { "Stop" } else { "Resume" });
        poll_queue_until(&mut app, &ctx, |app| !app.jobs.is_busy());
        let transferred = observed.recv_timeout(Duration::from_secs(5)).unwrap();
        let expected = if stop { b"old content" } else { b"new content" };
        assert_eq!(transferred, expected);
        assert_eq!(fs::read(source).unwrap(), b"new content");
    }
}

#[test]
fn transfer_speed_dialog_updates_the_running_job_and_persists_connection_limits() {
    use egui::accesskit::{Action as AxAction, ActionData, ActionRequest, TreeId};
    use std::fs;

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("payload.bin");
    let to = root.path().join("destination");
    let data = vec![0x38; 512 * 1024];
    fs::write(&source, &data).unwrap();
    fs::create_dir(&to).unwrap();
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let saved_connection = Endpoint {
        host: "transfer.example".into(),
        port: 2222,
        user: "test".into(),
        key: Some("/test/key-filename".into()),
        jump: None,
    };
    app.settings.sftp_profiles.push(Location::Sftp {
        endpoint: saved_connection.clone(),
        path: "/".into(),
    });
    assert_eq!(app.settings.transfer_limits.global_kib, 0);
    app.settings.transfer_limits.global_kib = 4;
    app.start_transfer(
        &ctx,
        "paced copy",
        vec![source.clone().into()],
        to.clone().into(),
        TransferKind::Copy,
        false,
    );
    poll_queue_until(&mut app, &ctx, |_| {
        fs::read_dir(&to).unwrap().any(|entry| {
            entry
                .unwrap()
                .path()
                .join("payload")
                .metadata()
                .is_ok_and(|meta| meta.len() > 0)
        })
    });

    let request = |target_node, action, data| {
        egui::Event::AccessKitActionRequest(ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node,
            data,
        })
    };
    operation_footer_frame(&mut app, &ctx, vec![]);
    let footer = operation_footer_frame(&mut app, &ctx, vec![]);
    let speed = footer
        .platform_output
        .accesskit_update
        .unwrap()
        .nodes
        .into_iter()
        .find(|(_, node)| node.label() == Some("Speed…"))
        .unwrap()
        .0;
    operation_footer_frame(&mut app, &ctx, vec![request(speed, AxAction::Click, None)]);
    assert!(matches!(&app.dialog, Some(Dialog::TransferLimits { .. })));

    let dialog_frame = |app: &mut PawCommander, events| {
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(960.0, 600.0),
                )),
                events,
                ..Default::default()
            },
            |ui| app.dialogs(ui.ctx()),
        )
        .platform_output
        .accesskit_update
        .unwrap()
    };
    dialog_frame(&mut app, vec![]);
    let dialog = dialog_frame(&mut app, vec![]);
    let field = |value| {
        dialog
            .nodes
            .iter()
            .find(|(_, node)| node.numeric_value() == Some(value))
            .unwrap()
            .0
    };
    let global = field(4.0);
    let endpoint = field(0.0);
    let save = dialog
        .nodes
        .iter()
        .find(|(_, node)| node.label() == Some("Save"))
        .unwrap()
        .0;
    dialog_frame(
        &mut app,
        vec![
            request(
                global,
                AxAction::SetValue,
                Some(ActionData::NumericValue(0.0)),
            ),
            request(
                endpoint,
                AxAction::SetValue,
                Some(ActionData::NumericValue(12.0)),
            ),
        ],
    );
    assert_eq!(app.settings.transfer_limits.global_kib, 4);
    dialog_frame(&mut app, vec![request(save, AxAction::Click, None)]);
    assert_eq!(app.settings.transfer_limits.global_kib, 0);
    assert!(app.dialog.is_none());
    poll_queue_until(&mut app, &ctx, |app| app.jobs.current.is_none());
    assert!(fs::read(to.join(source.file_name().unwrap())).unwrap() == data);

    let saved = serde_json::to_string(&app.settings.transfer_limits).unwrap();
    assert!(!saved.contains("key-filename"));
    let restored: crate::operations::TransferLimits = serde_json::from_str(&saved).unwrap();
    assert_eq!(restored.connection_kib(&saved_connection), 12);
}

#[test]
fn shift_escape_stops_a_conflict_without_starting_the_batch() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let target_dir = root.path().join("target");
    std::fs::create_dir(&source_dir).unwrap();
    std::fs::create_dir(&target_dir).unwrap();
    let source = source_dir.join("conflict.txt");
    let target = target_dir.join("conflict.txt");
    std::fs::write(&source, "source").unwrap();
    std::fs::write(&target, "old destination").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.start_transfer(
        &ctx,
        "test copy",
        vec![source.clone().into()],
        target_dir.into(),
        TransferKind::Copy,
        false,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !app.conflict_open() && Instant::now() < deadline {
        app.poll(&ctx);
        std::thread::yield_now();
    }
    assert!(app.conflict_open());
    operation_footer_frame(
        &mut app,
        &ctx,
        vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::SHIFT,
        }],
    );
    assert!(!app.conflict_open());
    assert!(app
        .jobs
        .current
        .as_ref()
        .unwrap()
        .progress
        .cancel
        .is_requested());
    while app.jobs.current.is_some() && Instant::now() < deadline {
        app.poll(&ctx);
        std::thread::yield_now();
    }
    assert!(app.jobs.current.is_none(), "conflict worker must return");
    let (notice, error) = app.notice.as_ref().unwrap();
    assert!(!error);
    assert!(
        notice.contains("Completed: 0. Skipped: 0. Remaining: 1."),
        "{notice}"
    );
    assert!(notice.contains(&source.display().to_string()));
    assert_eq!(std::fs::read_to_string(source).unwrap(), "source");
    assert_eq!(std::fs::read_to_string(target).unwrap(), "old destination");
}

#[test]
fn conflict_answer_wait_observes_stop_and_rejects_a_racing_replace() {
    let (reply, answer) = mpsc::channel();
    let cancellation = Progress::default().cancellation();
    reply
        .send(Answer {
            decision: Decision::Replace,
            apply_to_group: true,
        })
        .unwrap();
    cancellation.request();
    let result = wait_for_conflict_answer(&answer, &cancellation);
    assert_eq!(result.decision, Decision::Cancel);
    assert!(!result.apply_to_group);

    let (reply, answer) = mpsc::channel();
    let cancellation = Progress::default().cancellation();
    let worker_cancel = cancellation.clone();
    let (finished, result) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        finished
            .send(wait_for_conflict_answer(&answer, &worker_cancel))
            .unwrap();
    });
    cancellation.request();
    assert_eq!(
        result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .decision,
        Decision::Cancel
    );
    worker.join().unwrap();
    drop(reply);
}

#[test]
fn late_stop_keeps_final_success_and_consumes_committed_cut_events() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let committed: Location = root.path().join("committed").into();
    let pending: Location = root.path().join("pending").into();
    let clipboard = Clipboard::new(
        vec![committed.clone(), pending.clone()],
        true,
        ClipboardOrigin::App(None),
    );
    app.clipboard = Some(clipboard);
    let cut = app.capture_clipboard_cut().unwrap();
    let (result, receiver) = mpsc::channel();
    let (_, progress) = Progress::tracked();
    app.jobs.current = Some(Job {
        transfer: None,
        cut: Some((cut, vec![committed.clone(), pending.clone()])),
        label: "test move".into(),
        started: Instant::now(),
        result: receiver,
        undo: None,
        progress,
        #[cfg(target_os = "macos")]
        remote_save: false,
        #[cfg(target_os = "macos")]
        export_target: None,
    });
    let (events, receiver) = mpsc::channel();
    app.jobs.current.as_mut().unwrap().transfer = Some(TransferInteraction {
        events: receiver,
        conflict: None,
        can_pause: false,
    });
    let (next_started, next_observed) = mpsc::channel();
    let (next_release, next_wait) = mpsc::channel();
    app.start_progress_job(&ctx, "next job", move |_| {
        next_started.send(()).unwrap();
        next_wait.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok("next completed".into())
    });
    app.request_transfer_stop(&ctx);
    app.poll(&ctx);
    assert!(
        next_observed.try_recv().is_err(),
        "Stop cannot dispatch before terminal cleanup handoff"
    );
    let success = crate::i18n::transfer_finished(1, 0, &[], false, false, "/destination");
    events
        .send(TransferEvent::Moved(committed, vec![]))
        .unwrap();
    result
        .send(JobCompletion::Text(Ok(success.clone())))
        .unwrap();
    // A token request never rewrites the worker's already finalized outcome.
    app.request_transfer_stop(&ctx);
    app.poll(&ctx);
    assert_eq!(app.notice, Some((success, false)));
    assert_eq!(app.clipboard.as_ref().unwrap().paths, vec![pending]);
    assert_eq!(app.jobs.current.as_ref().unwrap().label, "next job");
    assert!(app.jobs.transfer().is_none());
    next_observed.recv_timeout(Duration::from_secs(2)).unwrap();
    next_release.send(()).unwrap();
    poll_queue_until(&mut app, &ctx, |app| app.jobs.current.is_none());
}

#[test]
fn operation_center_stays_live_during_a_stalled_copy_and_reports_the_error() {
    use std::io::{self, Read};

    struct SlowSource {
        first: bool,
        waiting: mpsc::Sender<()>,
        release: Receiver<()>,
    }

    impl Read for SlowSource {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.first {
                self.first = false;
                // Simulate enough elapsed I/O time to get a first rate estimate.
                std::thread::sleep(Duration::from_millis(120));
                buffer[..4].copy_from_slice(b"part");
                return Ok(4);
            }
            self.waiting.send(()).unwrap();
            let _ = self.release.recv();
            Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "test disconnect",
            ))
        }
    }

    fn draw_footer(app: &mut PawCommander, ctx: &egui::Context) -> String {
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 600.0),
                )),
                ..Default::default()
            },
            |ui| {
                let ctx = &ui.ctx().clone();
                app.poll(ctx);
                app.footer(ui);
            },
        )
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text.galley.text()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
    }

    let root = tempfile::tempdir().unwrap();
    let other = root.path().join("another folder");
    std::fs::create_dir(&other).unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let (waiting, blocked) = mpsc::channel();
    let (resume, release) = mpsc::channel();
    app.start_progress_job(
        &ctx,
        "Копирование файлов",
        move |progress| {
            let mut input = SlowSource {
                first: true,
                waiting,
                release,
            };
            progress
                .copy(Path::new("file.txt"), Some(16), &mut input, &mut io::sink())
                .map(|_| "Готово".into())
                .map_err(|error| error.to_string())
        },
    );
    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
    // The bottom panel settles its measured height after its first frame.
    draw_footer(&mut app, &ctx);
    let active = draw_footer(&mut app, &ctx);
    assert!(active.contains("Current item: file.txt"), "{active}");
    assert!(active.contains("4 B / 16 B"), "{active}");
    assert!(active.contains("B/s"), "{active}");
    assert!(active.contains("File remaining: ≈"), "{active}");
    app.action(Action::Navigate(0, other.clone().into()), &ctx);
    assert_eq!(app.tab().path.path(), other);
    assert!(app.jobs.current.is_some());

    resume.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.jobs.current.is_some() && Instant::now() < deadline {
        app.poll(&ctx);
        std::thread::yield_now();
    }
    assert!(app.jobs.current.is_none());
    assert!(draw_footer(&mut app, &ctx).contains("test disconnect"));
}
