use super::*;
use std::fs;

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
        "ProxyJump · line 7",
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
    click_text(&mut app, second.to_str().unwrap());
    assert_eq!(app.sftp_form.as_ref().unwrap().endpoint().key, Some(second));
    assert!(app.connection.is_none());
}
