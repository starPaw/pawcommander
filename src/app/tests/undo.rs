use super::*;

#[test]
fn history_undo_owns_one_queued_request_survives_remove_and_spawn_failure() {
    let root = tempfile::tempdir().unwrap();
    let source: Location = root.path().join("original").into();
    std::fs::write(source.path(), b"original bytes").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let history_path = root.path().join("history.json");
    app.history = crate::result_history::History::for_test(history_path.clone(), &ctx);
    let original = source.clone();
    app.start_progress_job(&ctx, "rename", move |progress| {
        crate::vfs::rename_with_undo(&original, "renamed", progress)
            .map(|path| path.display())
            .map_err(|error| crate::i18n::file_error(&error))
    });
    poll_queue_until(&mut app, &ctx, |app| app.jobs.current.is_none());
    assert_eq!(app.jobs.undo.len(), 1);
    let id = app.jobs.undo[0].id.clone();
    assert_eq!(app.history.undoable[&id], 1);
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&history_path).unwrap()).unwrap();
    assert_eq!(saved["records"][0]["id"], id);
    let held = hold_queue(&mut app);
    app.undo_operation(&ctx, &id);
    app.undo_operation(&ctx, &id);
    assert_eq!(app.jobs.pending.len(), 1);
    assert!(!app.history.undoable.contains_key(&id));
    app.remove_pending_job(0);
    assert_eq!(app.history.undoable[&id], 1);
    app.undo_operation(&ctx, &id);
    app.jobs.fail_spawn = true;
    held.send(JobCompletion::text(Ok("release".into())))
        .unwrap();
    poll_queue_until(&mut app, &ctx, |app| app.jobs.start_failed);
    assert_eq!(app.jobs.pending.len(), 1);
    assert!(!source.path().exists());
    app.jobs.fail_spawn = false;
    app.jobs.start_failed = false;
    app.start_next_job(&ctx);
    poll_queue_until(&mut app, &ctx, |app| app.jobs.current.is_none());
    assert_eq!(std::fs::read(source.path()).unwrap(), b"original bytes");
    assert!(!root.path().join("renamed").exists());
    assert!(app.jobs.undo.is_empty());
    assert!(!app.history.undoable.contains_key(&id));
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&history_path).unwrap()).unwrap();
    assert_eq!(saved["records"].as_array().unwrap().len(), 2);
    assert_eq!(saved["records"][1]["items"][0]["outcome"], "Completed");
    let restarted = PawCommander::new(&cc, Some(root.path().into()));
    assert!(restarted.jobs.undo.is_empty() && restarted.history.undoable.is_empty());
}

#[test]
fn history_undo_rechecks_at_dispatch_and_retains_only_failed_work() {
    let root = tempfile::tempdir().unwrap();
    let source: Location = root.path().join("source").into();
    let target: Location = root.path().join("target").into();
    std::fs::create_dir(source.path()).unwrap();
    std::fs::create_dir(target.path()).unwrap();
    std::fs::write(source.join("a").path(), b"a").unwrap();
    std::fs::write(source.join("b").path(), b"b").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(source.path().into()));
    app.history = crate::result_history::History::for_test(root.path().join("history.json"), &ctx);
    app.start_transfer(
        &ctx,
        "move",
        vec![source.join("a"), source.join("b")],
        target.clone(),
        TransferKind::Move,
        false,
    );
    poll_queue_until(&mut app, &ctx, |app| app.jobs.current.is_none());
    assert_eq!(app.jobs.undo.len(), 1);
    let id = app.jobs.undo[0].id.clone();
    let held = hold_queue(&mut app);
    app.undo_operation(&ctx, &id);
    std::fs::write(source.join("a").path(), b"external occupant").unwrap();
    held.send(JobCompletion::text(Ok("release".into())))
        .unwrap();
    poll_queue_until(&mut app, &ctx, |app| !app.jobs.is_busy());
    assert_eq!(
        std::fs::read(source.join("a").path()).unwrap(),
        b"external occupant"
    );
    assert_eq!(std::fs::read(source.join("b").path()).unwrap(), b"b");
    assert_eq!(std::fs::read(target.join("a").path()).unwrap(), b"a");
    assert_eq!(app.history.undoable[&id], 1);
    std::fs::remove_file(source.join("a").path()).unwrap();
    std::fs::write(source.join("b").path(), b"edited after successful undo").unwrap();
    app.undo_operation(&ctx, &id);
    poll_queue_until(&mut app, &ctx, |app| app.jobs.current.is_none());
    assert_eq!(std::fs::read(source.join("a").path()).unwrap(), b"a");
    assert_eq!(
        std::fs::read(source.join("b").path()).unwrap(),
        b"edited after successful undo"
    );
    assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
    assert!(app.jobs.undo.is_empty());
}

#[test]
fn history_undo_is_not_offered_when_result_persistence_failed() {
    let root = tempfile::tempdir().unwrap();
    let source: Location = root.path().join("source").into();
    std::fs::write(source.path(), b"data").unwrap();
    let blocker = root.path().join("not-a-directory");
    std::fs::write(&blocker, b"unrelated").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.history = crate::result_history::History::for_test(blocker.join("history.json"), &ctx);
    app.start_progress_job(&ctx, "rename", move |progress| {
        crate::vfs::rename_with_undo(&source, "renamed", progress)
            .map(|path| path.display())
            .map_err(|error| crate::i18n::file_error(&error))
    });
    poll_queue_until(&mut app, &ctx, |app| app.jobs.current.is_none());
    assert!(app.jobs.undo.is_empty() && app.history.undoable.is_empty());
    assert_eq!(std::fs::read(root.path().join("renamed")).unwrap(), b"data");
    assert_eq!(std::fs::read(blocker).unwrap(), b"unrelated");
}
