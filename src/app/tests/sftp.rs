use super::*;
use std::fs;

#[test]
fn permissions_command_requires_an_sftp_selection_and_preview() {
    let fixture = tempfile::tempdir().unwrap();
    let local = Location::Local(fixture.path().join("local.txt"));
    fs::write(local.path(), b"local fixture").unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(fixture.path().into()));
    wait_for_directory(app.tab_mut());
    app.tab_mut().selected.insert(local);
    app.run_shortcut(Command::ChangePermissions, &ctx);
    assert!(app.dialog.is_none());

    let tab = app.tab_mut();
    tab.path = Location::Sftp {
        endpoint: Endpoint {
            host: "127.0.0.1".into(),
            port: 0,
            user: "fixture".into(),
            key: None,
            jump: None,
        },
        path: "/data".into(),
    };
    let selected = tab.path.join("sample.txt");
    tab.entries.replace(vec![FileEntry {
        name: selected.name(),
        path: selected.clone(),
        is_dir: false,
        is_symlink: false,
        size: 1,
        modified: None,
        metadata: Default::default(),
    }]);
    tab.selected = BTreeSet::from([selected]);
    app.run_shortcut(Command::ChangePermissions, &ctx);
    assert!(matches!(&app.dialog, Some(Dialog::ChangePermissions(editor)) if !editor.can_apply()));

    let mut draw = || {
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(960.0, 680.0),
                )),
                ..Default::default()
            },
            |ui| app.dialogs(ui.ctx()),
        )
    };
    draw();
    let rendered = operation_footer_text(&draw());
    assert!(
        rendered.contains(Command::ChangePermissions.label()),
        "{rendered}"
    );
    assert!(
        rendered.contains(crate::i18n::text("Предпросмотр")),
        "{rendered}"
    );
    assert!(app.job.is_none());
}

#[test]
fn ssh_config_form_reviews_the_effective_endpoint_and_selects_an_identity() {
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().to_owned();
    let ssh = home.join(".ssh");
    fs::create_dir(&ssh).unwrap();
    fs::write(
        ssh.join("config"),
        "Host project\nHostName build.example\nUser deploy\nPort 2202\nIdentityFile \"~/.ssh/default key\"\nIdentityFile ~/.ssh/second\nProxyJump bastion\n",
    )
    .unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = PawCommander::new(&cc, Some(fixture.path().into()));
    let mut form = SftpForm {
        host: "project".into(),
        path: "/work".into(),
        password: "previous password".into(),
        passphrase: "previous passphrase".into(),
        save_key: true,
        ..SftpForm::default()
    };
    form.start_ssh_config(&ctx, move |alias| {
        crate::ssh_config::load_config(&home, alias, "fixture-user")
    });
    app.sftp_form = Some(form);

    let draw = |app: &mut PawCommander, events| {
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(960.0, 680.0),
                )),
                events,
                ..Default::default()
            },
            |ui| app.sftp_dialog(ui.ctx()),
        )
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.sftp_form.as_ref().unwrap().ssh_config_job.is_some() && Instant::now() < deadline {
        draw(&mut app, vec![]);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(app.sftp_form.as_ref().unwrap().error.is_none());
    let form = app.sftp_form.as_ref().unwrap();
    assert_eq!(
        form.endpoint(),
        Endpoint {
            host: "build.example".into(),
            user: "deploy".into(),
            port: 2202,
            key: Some(ssh.join("default key")),
            #[cfg(target_os = "macos")]
            jump: Some(Box::new(crate::vfs::JumpHost {
                host: "bastion".into(),
                port: 22,
                user: "fixture-user".into(),
                key: None,
            })),
            #[cfg(not(target_os = "macos"))]
            jump: None,
        }
    );
    assert_eq!(form.path, "/work");
    assert!(form.password.is_empty() && form.passphrase.is_empty() && !form.save_key);
    assert!(app.connection.is_none());

    draw(&mut app, vec![]);
    let rendered = operation_footer_text(&draw(&mut app, vec![]));
    for label in [
        "Load SSH alias",
        "Loaded SSH alias project",
        #[cfg(target_os = "macos")]
        "ProxyJump · fixture-user@bastion · 22",
        #[cfg(target_os = "macos")]
        "Connect via ProxyJump",
        #[cfg(not(target_os = "macos"))]
        "ProxyJump · line 7",
        #[cfg(not(target_os = "macos"))]
        "Connect directly",
    ] {
        assert!(rendered.contains(label), "missing {label} in {rendered}");
    }

    let click_text = |app: &mut PawCommander, label: &str| {
        draw(app, vec![]);
        let output = draw(app, vec![]);
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing {label} in {}", operation_footer_text(&output)));
        for pressed in [true, false] {
            draw(
                app,
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
    click_text(&mut app, "Key files from SSH config");
    let second = ssh.join("second");
    app.sftp_form.as_mut().unwrap().passphrase = "first key only".into();
    click_text(&mut app, second.to_str().unwrap());
    assert_eq!(app.sftp_form.as_ref().unwrap().endpoint().key, Some(second));
    assert!(app.sftp_form.as_ref().unwrap().passphrase.is_empty());
    assert!(app.connection.is_none());

    #[cfg(target_os = "macos")]
    {
        let message = "The SSH host key changed. Saved fingerprint SHA256:previous. Received SHA256:rotated. Verify the fingerprint with the administrator and review the saved key.";
        app.sftp_form.as_mut().unwrap().error = Some(message.repeat(3));
        draw(&mut app, vec![]);
        let rendered = operation_footer_text(&draw(&mut app, vec![]));
        assert!(rendered.contains("SHA256:previous") && rendered.contains("SHA256:rotated"));
        assert!(rendered.contains("Cancel") && rendered.contains("Connect via ProxyJump"));
    }
}
