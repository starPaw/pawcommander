use super::*;

#[test]
fn language_settings_preserve_legacy_preferences_and_round_trip() {
    let mut settings: super::Settings =
        serde_json::from_str(r#"{"hidden":true,"split":false}"#).unwrap();
    assert_eq!(settings.language, crate::i18n::Language::English);
    assert!(settings.hidden);
    assert!(!settings.split);
    settings.language = crate::i18n::Language::Russian;
    let restored: super::Settings =
        serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
    assert_eq!(restored.language, crate::i18n::Language::Russian);
    assert!(restored.hidden);
    assert!(!restored.split);
}

#[test]
fn settings_round_trip_shortcut_profile_and_overrides() {
    let mut storage = MemoryStorage::default();
    let settings: Settings = serde_json::from_value(serde_json::json!({
        "shortcuts": {
            "profile": "Commander",
            "overrides": {
                "Rename": {
                    "modifiers": {
                        "alt": false,
                        "ctrl": false,
                        "shift": false,
                        "mac_cmd": false,
                        "command": false
                    },
                    "logical_key": "F9"
                },
                "Refresh": null
            }
        }
    }))
    .unwrap();
    eframe::set_value(&mut storage, "settings", &settings);
    let mut restored: Settings = eframe::get_value(&storage, "settings").unwrap();
    restored.shortcuts.validate();
    assert_eq!(restored.shortcuts, settings.shortcuts);
    for (key, command) in [
        (egui::Key::F2, None),
        (egui::Key::F5, Some(Command::CopyToPane)),
        (egui::Key::F9, Some(Command::Rename)),
    ] {
        assert_eq!(
            restored
                .shortcuts
                .command(shortcuts::chord(key, egui::Modifiers::NONE)),
            command
        );
    }
    assert_eq!(
        restored
            .shortcuts
            .command(shortcuts::chord(egui::Key::R, egui::Modifiers::COMMAND)),
        None
    );
}

#[test]
fn workspace_session_restores_selection_sort_and_back_forward_with_missing_path() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let base: Location = root.path().to_path_buf().into();
    let left = base.join("left");
    let forward = base.join("forward");
    let missing = base.join("missing");
    for directory in [&left, &forward, &missing] {
        std::fs::create_dir(directory.path()).unwrap();
    }
    std::fs::write(left.join("present").path(), "data").unwrap();

    app.action(Action::TogglePinTab(0, 0), &ctx);
    app.action(Action::NewTab(0), &ctx);
    app.action(Action::Navigate(0, left.clone()), &ctx);
    app.action(Action::Navigate(0, forward.clone()), &ctx);
    app.action(Action::Back, &ctx);
    app.panes[0].tab_mut().sort = Sort::Size;
    app.panes[0].tab_mut().descending = true;
    app.action(
        app.folder_view_action(0, FolderViewChange::Mode(FolderMode::Grid)),
        &ctx,
    );
    app.action(Action::NewTab(1), &ctx);
    app.action(Action::Navigate(1, missing.clone()), &ctx);
    app.panes[1].tab_mut().sort = Sort::Modified;

    let mut storage = MemoryStorage::default();
    eframe::App::save(&mut app, &mut storage);
    std::fs::remove_dir(missing.path()).unwrap();
    cc.storage = Some(&storage);
    let mut reopened = PawCommander::new(&cc, None);
    assert_eq!(reopened.active, 1);
    assert_eq!(reopened.panes[0].pinned_count(), 1);
    assert_eq!(reopened.panes[0].current, 1);
    assert_eq!(reopened.panes[1].current, 1);
    assert_eq!(reopened.panes[0].tab().path, left);
    assert_eq!(reopened.tab().path, missing);
    assert!(reopened.panes[0].tab().sort == Sort::Size);
    assert!(reopened.panes[0].tab().descending);
    assert!(reopened.tab().sort == Sort::Modified);
    assert!(!reopened.tab().descending);
    assert_eq!(reopened.folder_view(0).mode, FolderMode::Grid);
    assert_eq!(reopened.folder_view(1).mode, FolderMode::List);
    wait_for_directory(reopened.panes[0].tab_mut());
    wait_for_directory(reopened.tab_mut());
    assert_eq!(reopened.panes[0].tab().entries[0].name, "present");
    assert!(reopened.tab().error.is_some());
    reopened.action(Action::History(0, -1), &ctx);
    assert_eq!(reopened.tab().path, base);
    reopened.action(Action::Forward, &ctx);
    assert_eq!(reopened.tab().path, left);
    reopened.action(Action::Forward, &ctx);
    assert_eq!(reopened.tab().path, forward);
    reopened.action(Action::History(1, -1), &ctx);
    assert_eq!(reopened.tab().path, base);

    let explicit = PawCommander::new(&cc, Some(forward.path().to_path_buf()));
    assert_eq!(explicit.active, 0);
    for pane in &explicit.panes {
        assert_eq!(pane.tabs.len(), 1);
        assert_eq!(pane.tab().path, forward);
        assert_eq!(pane.tab().history, vec![forward.clone()]);
        assert!(!pane.tab().pinned);
        assert!(pane.tab().sort == Sort::Name);
    }

    eframe::Storage::set_string(
        &mut storage,
        PawCommander::SESSION_KEY,
        "invalid session".into(),
    );
    let mut fallback_cc = eframe::CreationContext::_new_kittest(ctx.clone());
    fallback_cc.storage = Some(&storage);
    let legacy_fallback = PawCommander::new(&fallback_cc, None);
    assert_eq!(legacy_fallback.panes[0].pinned_count(), 1);
    assert_eq!(legacy_fallback.panes[0].tabs[1].path, left);
    assert_eq!(legacy_fallback.panes[1].tabs[1].path, missing);
    assert_eq!(
        legacy_fallback.panes[0].tabs[1]
            .folder_view(&legacy_fallback.settings, 0, &legacy_fallback.folder_views)
            .mode,
        FolderMode::Grid,
    );
}

#[test]
fn session_preserves_native_local_paths_and_remote_coordinates() {
    #[cfg(unix)]
    let native = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![b'x', 0xff])
    };
    #[cfg(windows)]
    let native = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[u16::from(b'x'), 0xd800])
    };
    let root = tempfile::tempdir().unwrap();
    let local: Location = root.path().join(&native).into();
    let remote = Location::Sftp {
        endpoint: Endpoint {
            host: "example.invalid".into(),
            port: 2022,
            user: "user".into(),
            key: Some(root.path().join(&native)),
            #[cfg(target_os = "macos")]
            jump: Some(Box::new(crate::vfs::JumpHost {
                host: "jump.invalid".into(),
                port: 22,
                user: "relay".into(),
                key: Some(root.path().join(&native)),
            })),
            #[cfg(not(target_os = "macos"))]
            jump: None,
        },
        path: PathBuf::from("/folder").join(native),
    };
    let ctx = egui::Context::default();
    let mut cc = eframe::CreationContext::_new_kittest(ctx);
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.tab_mut().navigate(local.clone(), &cc.egui_ctx);
    app.tab_mut().history.push(remote.clone());
    app.tab_mut().pinned = true;
    app.settings.hidden = true;
    let mut storage = MemoryStorage::default();
    eframe::App::save(&mut app, &mut storage);
    let legacy: Settings = eframe::get_value(&storage, PawCommander::SETTINGS_KEY).unwrap();
    assert!(legacy.locations[0].is_empty());
    assert_eq!(legacy.pinned_tab_counts, [0, 0]);
    cc.storage = Some(&storage);
    let reopened = PawCommander::new(&cc, None);
    assert!(reopened.settings.hidden);
    assert_eq!(reopened.tab().path, local);
    assert_eq!(reopened.tab().history[2], remote);
    assert!(reopened.tab().pinned);
}

#[test]
fn named_workspace_opens_both_folders_in_existing_tabs_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let base: Location = root.path().to_path_buf().into();
    let left = base.join("left");
    let right = base.join("right");
    for folder in [&left, &right] {
        std::fs::create_dir(folder.path()).unwrap();
    }
    let ctx = egui::Context::default();
    let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    app.action(Action::Navigate(0, left.clone()), &ctx);
    app.action(Action::Navigate(1, right.clone()), &ctx);
    app.save_workspace_name(None, "  Daily work  ".into());

    app.action(Action::NewTab(0), &ctx);
    app.action(Action::TogglePinTab(0, 1), &ctx);
    app.action(Action::Navigate(0, base.clone()), &ctx);
    app.action(Action::Navigate(1, base.clone()), &ctx);
    app.toggle_split();
    let mut storage = MemoryStorage::default();
    eframe::App::save(&mut app, &mut storage);
    cc.storage = Some(&storage);
    let mut reopened = PawCommander::new(&cc, None);
    assert!(!reopened.settings.split);
    assert_eq!(reopened.workspaces[0].name, "Daily work");
    let tab_ids: Vec<_> = reopened.panes.iter().map(|pane| pane.tab().id).collect();

    reopened.action(Action::OpenWorkspace(0), &ctx);
    assert!(reopened.settings.split);
    assert_eq!(reopened.active, 0);
    assert_eq!(reopened.panes[0].tabs.len(), 2);
    assert_eq!(reopened.panes[1].tabs.len(), 1);
    assert!(reopened.tab().pinned);
    assert_eq!(reopened.tab().path, left);
    assert_eq!(reopened.panes[1].tab().path, right);
    for (pane, previous_id) in reopened.panes.iter().zip(tab_ids) {
        assert_eq!(pane.tab().id, previous_id);
    }
    reopened.action(Action::Back, &ctx);
    assert_eq!(reopened.tab().path, base);
    reopened.action(Action::History(1, -1), &ctx);
    assert_eq!(reopened.tab().path, base);
}

#[cfg(target_os = "macos")]
#[test]
fn cli_workspace_loads_the_saved_pair_instead_of_the_last_session() {
    use super::super::{find_saved_workspace, Startup, WorkspaceError};

    let root = tempfile::tempdir().unwrap();
    let folders = ["First project", "Вторая папка"].map(|name| {
        let folder = root.path().join(name);
        std::fs::create_dir(&folder).unwrap();
        Location::from(folder)
    });
    let ctx = egui::Context::default();
    let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().to_owned()));
    for (pane, path) in folders.iter().enumerate() {
        app.action(Action::Navigate(pane, path.clone()), &ctx);
    }
    app.save_workspace_name(None, "Two named folders".into());
    for pane in 0..2 {
        app.action(Action::Navigate(pane, root.path().to_owned().into()), &ctx);
    }
    app.settings.split = false;
    let mut storage = MemoryStorage::default();
    eframe::App::save(&mut app, &mut storage);
    let profile = ron::to_string(&storage.0).unwrap();
    let saved = find_saved_workspace(Some(profile.as_bytes()), "Two named folders").unwrap();
    cc.storage = Some(&storage);
    let opened = PawCommander::start(&cc, Startup::Workspace(saved));
    assert!(opened.settings.split);
    for (pane, path) in opened.panes.iter().zip(folders) {
        assert_eq!(pane.tab().path, path);
    }
    assert_eq!(
        find_saved_workspace(Some(profile.as_bytes()), "Missing"),
        Err(WorkspaceError::Missing)
    );
    assert_eq!(
        find_saved_workspace(Some(b"broken profile"), "Missing"),
        Err(WorkspaceError::Corrupt)
    );
}

#[test]
fn bookmark_and_workspace_settings_round_trip_native_paths() {
    #[cfg(unix)]
    let native = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![b'x', 0xff])
    };
    #[cfg(windows)]
    let native = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[u16::from(b'x'), 0xd800])
    };
    let root = tempfile::tempdir().unwrap();
    let native_folder: Location = root.path().join(native).into();
    let ctx = egui::Context::default();
    let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let mut storage = MemoryStorage::default();
    eframe::App::save(&mut app, &mut storage);
    app.bookmarks.push(Bookmark::new(native_folder.clone()));
    app.settings.ui_scale = 125;
    app.action(Action::Navigate(1, native_folder.clone()), &ctx);
    app.save_workspace_name(None, "Native folders".into());
    eframe::App::save(&mut app, &mut storage);
    assert_ne!(storage.0[PawCommander::SETTINGS_KEY], "{}");

    cc.storage = Some(&storage);
    let reopened = PawCommander::new(&cc, Some(root.path().into()));
    assert_eq!(reopened.bookmarks[0].location, native_folder);
    assert_eq!(reopened.settings.ui_scale, 125);
    assert_eq!(reopened.workspaces[0].name, "Native folders");
    assert_eq!(
        reopened.workspaces[0].folders[1].clone().into_location(),
        Some(native_folder)
    );
}

#[test]
fn named_workspace_dialog_rejects_duplicates_and_supports_rename_and_remove() {
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    let enter = || egui::RawInput {
        events: [true, false]
            .map(|pressed| egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            })
            .to_vec(),
        ..Default::default()
    };
    let submit = |app: &mut PawCommander, name: &str| {
        let Some(Dialog::WorkspaceName { name: field, .. }) = &mut app.dialog else {
            panic!("expected workspace dialog");
        };
        *field = name.into();
        let _ = ctx.run_test_ui(enter(), |ui| app.dialogs(ui.ctx()));
    };

    app.action(Action::SaveWorkspace, &ctx);
    submit(&mut app, "   ");
    assert!(app.dialog.is_some());
    assert!(app.workspaces.is_empty());
    submit(&mut app, " Daily ");
    assert!(app.dialog.is_none());
    app.action(Action::SaveWorkspace, &ctx);
    submit(&mut app, "Daily");
    assert!(app.dialog.is_some());
    assert_eq!(app.workspaces.len(), 1);
    submit(&mut app, "Archive");
    assert!(app.dialog.is_none());

    let _ = ctx.run_test_ui(enter(), |ui| {
        let ctx = &ui.ctx().clone();
        app.action(Action::RenameWorkspace(0), ctx);
        app.dialogs(ctx);
    });
    assert!(matches!(
        &app.dialog,
        Some(Dialog::WorkspaceName { editing: Some(0), name }) if name == "Daily"
    ));
    submit(&mut app, "Archive");
    assert!(app.dialog.is_some());
    submit(&mut app, " Daily ");
    assert!(app.dialog.is_none());
    app.action(Action::RenameWorkspace(0), &ctx);
    let _ = ctx.run_test_ui(egui::RawInput::default(), |ui| app.dialogs(ui.ctx()));
    submit(&mut app, "Regular");
    assert_eq!(app.workspaces[0].name, "Regular");
    assert_eq!(app.workspaces[1].name, "Archive");
    app.action(Action::RemoveWorkspace(0), &ctx);
    assert_eq!(app.workspaces.len(), 1);
    assert_eq!(app.workspaces[0].name, "Archive");
}

#[test]
fn named_workspace_persists_sftp_coordinates_without_credentials() {
    let root = tempfile::tempdir().unwrap();
    let local: Location = root.path().to_path_buf().into();
    let key = root.path().join("disposable-key");
    std::fs::write(&key, "synthetic-private-key-material").unwrap();
    let remote = Location::Sftp {
        endpoint: Endpoint {
            host: "workspace-test.invalid".into(),
            port: 2022,
            user: "workspace-test".into(),
            key: Some(key),
            #[cfg(target_os = "macos")]
            jump: Some(Box::new(crate::vfs::JumpHost {
                host: "jump.invalid".into(),
                port: 22,
                user: "relay".into(),
                key: Some(root.path().join("jump-key")),
            })),
            #[cfg(not(target_os = "macos"))]
            jump: None,
        },
        path: PathBuf::from("/folder/папка"),
    };
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(root.path().into()));
    // Supply the location without starting a network or keyring request.
    app.panes[1].tab_mut().path = remote.clone();
    app.settings.sftp_profiles = vec![remote.clone()];
    app.sftp_form = Some(SftpForm {
        password: "synthetic-account-password".into(),
        passphrase: "synthetic-key-passphrase".into(),
        #[cfg(target_os = "macos")]
        jump_passphrase: "synthetic-jump-passphrase".into(),
        save_key: true,
        ..Default::default()
    });
    app.save_workspace_name(None, "With SFTP".into());
    let mut storage = MemoryStorage::default();
    eframe::App::save(&mut app, &mut storage);
    let stored: Settings = eframe::get_value(&storage, PawCommander::SETTINGS_KEY).unwrap();
    let workspaces: Vec<NamedWorkspace> =
        eframe::get_value(&storage, PawCommander::WORKSPACES_KEY).unwrap();
    let folders = workspaces[0]
        .folders
        .clone()
        .map(SavedLocation::into_location);
    assert_eq!(folders, [Some(local), Some(remote.clone())]);
    assert_eq!(stored.sftp_profiles, vec![remote.clone()]);
    for payload in storage.0.values() {
        for secret in [
            "synthetic-private-key-material",
            "synthetic-account-password",
            "synthetic-key-passphrase",
            "synthetic-jump-passphrase",
        ] {
            assert!(!payload.contains(secret));
        }
    }

    let loopback = Location::Sftp {
        endpoint: Endpoint {
            host: "127.0.0.1".into(),
            port: 0,
            user: "test".into(),
            key: None,
            jump: None,
        },
        path: PathBuf::from("/workspace"),
    };
    app.sftp_form = None;
    app.panes[1].tab_mut().path = loopback.clone();
    app.save_workspace_name(None, "Loopback".into());
    app.action(Action::Navigate(1, root.path().to_path_buf().into()), &ctx);
    app.action(Action::OpenWorkspace(1), &ctx);
    assert_eq!(app.panes[1].tab().path, loopback);
    assert_eq!(app.settings.sftp_profiles, vec![remote]);
}

#[test]
fn settings_preserve_remote_tabs_and_read_legacy_local_paths() {
    let mut storage = MemoryStorage::default();
    #[derive(Serialize)]
    struct Legacy {
        locations: Vec<Vec<PathBuf>>,
        bookmarks: Vec<PathBuf>,
    }
    eframe::set_value(
        &mut storage,
        "legacy",
        &Legacy {
            locations: vec![vec![PathBuf::from("/tmp/two words")]],
            bookmarks: vec![PathBuf::from("/tmp")],
        },
    );
    let legacy: Settings = eframe::get_value(&storage, "legacy").expect("legacy settings");
    assert_eq!(
        legacy.locations[0][0],
        Location::Local(PathBuf::from("/tmp/two words"))
    );
    assert_eq!(legacy.theme, Theme::Frappe);
    assert_eq!(legacy.shortcuts, Shortcuts::default());
    assert_eq!(legacy.pinned_tab_counts, [0; 2]);
    let remote =
        fs_ops::resolve("sftp://alice@example.com:2222/папка", &legacy.bookmarks[0]).unwrap();
    let settings = Settings {
        locations: vec![vec![remote.clone()]],
        bookmarks: vec![remote.clone()],
        theme: Theme::Dark,
        sftp_profiles: vec![remote.clone()],
        ..Default::default()
    };
    eframe::set_value(&mut storage, "new", &settings);
    let restored: Settings = eframe::get_value(&storage, "new").expect("SFTP settings");
    assert_eq!(restored.locations[0][0], remote);
    assert_eq!(restored.bookmarks[0], remote);
    assert_eq!(restored.theme, Theme::Dark);
    assert_eq!(restored.sftp_profiles, vec![remote]);
}
