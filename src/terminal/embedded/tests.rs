use super::*;
use crate::test_ui::ContextExt as _;
use crate::vfs::Endpoint;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::TermMode;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn visible(session: &Session) -> String {
    session
        .terminal
        .lock()
        .renderable_content()
        .display_iter
        .map(|cell| cell.c)
        .collect()
}

fn until(session: &mut Session, matches: impl Fn(&Session) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        session.poll();
        if matches(session) {
            return;
        }
        if let State::Failed(error) = &session.state {
            panic!("terminal failed: {error}");
        }
        assert!(
            Instant::now() < deadline,
            "terminal output: {}",
            visible(session)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn embedded_terminal_runs_in_the_exact_local_folder_with_pty_input_and_resize() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("reports ' draft & notes\n");
    std::fs::create_dir(&folder).unwrap();
    let ctx = egui::Context::default();
    let mut terminal = Session::start_with(&ctx, move || {
        let mut command = Command::new("/bin/sh");
        command
            .current_dir(&folder)
            .args([
                "-c",
                r#"
            test -t 0 && test -t 1 && test -t 2 && test . -ef "$1" || exit 41
            printf '\033[31mPAW_READY\033[0m\n'
            IFS= read -r text || exit 42
            printf 'PAW_DIMENSIONS '; /bin/stty size
            printf 'PAW_INPUT=%s\n' "$text"
        "#,
                "fixture",
            ])
            .arg(&folder);
        Ok(command)
    })
    .unwrap();
    until(&mut terminal, |terminal| {
        visible(terminal).contains("PAW_READY")
    });
    assert!(terminal
        .terminal
        .lock()
        .renderable_content()
        .display_iter
        .any(|cell| {
            cell.c == 'P'
                && cell.fg
                    == alacritty_terminal::vte::ansi::Color::Named(
                        alacritty_terminal::vte::ansi::NamedColor::Red,
                    )
        }));
    terminal.resize(session::Size {
        rows: 37,
        columns: 92,
        ..session::Size::default()
    });
    assert!(terminal.input("hello from терминал\r".as_bytes()));
    until(&mut terminal, |terminal| terminal.state.finished());
    assert!(matches!(terminal.state, State::Exited(status) if status.success()));
    let screen = visible(&terminal);
    assert!(screen.contains("PAW_DIMENSIONS 37 92"), "{screen}");
    assert!(screen.contains("PAW_INPUT=hello from терминал"), "{screen}");
    assert_eq!(terminal.terminal.lock().columns(), 92);
}

#[test]
fn embedded_terminal_close_reaps_shell_and_stops_its_foreground_group() {
    let root = tempfile::tempdir().unwrap();
    let ready = root.path().join("pids");
    let folder = root.path().to_path_buf();
    let ctx = egui::Context::default();
    let mut terminal = Session::start_with(&ctx, move || {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-i")
            .current_dir(folder)
            .env("ENV", "/dev/null");
        Ok(command)
    })
    .unwrap();
    let read_pids = || {
        std::fs::read_to_string(&ready)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.parse::<libc::pid_t>().ok())
            .collect::<Vec<_>>()
    };
    assert!(terminal.input(br#"/bin/sh -c 'trap "" HUP TERM; printf "%s\n%s\n" "$PPID" "$$" > pids; exec /bin/sleep 30'"#));
    assert!(terminal.input(b"\r"));
    until(&mut terminal, |_| read_pids().len() == 2);
    let pids = read_pids();
    assert!(pids.iter().all(|&pid| pid > 1));
    // SAFETY: getpgid inspects only the test's disposable foreground job.
    assert_eq!(unsafe { libc::getpgid(pids[1]) }, pids[1]);
    terminal.stop();
    until(&mut terminal, |terminal| terminal.state.finished());
    assert!(matches!(terminal.state, State::Stopped));
    // SAFETY: waitpid targets the disposable shell created by this test.
    assert_eq!(
        unsafe { libc::waitpid(pids[0], std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    until(&mut terminal, |_| {
        pids.iter().all(|&pid| {
            // SAFETY: signal 0 only probes liveness for the disposable process IDs.
            (unsafe { libc::kill(pid, 0) }) != 0
        })
    });
}

#[test]
fn embedded_terminal_focus_consumes_file_shortcuts_and_sends_terminal_keys() {
    let root = tempfile::tempdir().unwrap();
    let folder = Location::Local(root.path().join("captured folder"));
    let ctx = egui::Context::default();
    let session = Session::start_with(&ctx, || {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "IFS= read -r line; printf 'KEYS=%s\\n' \"$line\""]);
        Ok(command)
    })
    .unwrap();
    let mut panel = Panel {
        folder,
        session,
        close_requested: false,
        focus_requested: true,
        copy: None,
        input_full: false,
        scroll: 0.0,
    };
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(960.0, 630.0));
    for _ in 0..2 {
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ui| panel.show(ui, true),
        );
    }
    assert!(ctx.memory(|memory| memory.has_focus(input_id())));
    let press = |key| egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    let mut events = vec![
        press(egui::Key::F8),
        egui::Event::Text("typing".into()),
        press(egui::Key::Enter),
    ];
    assert!(panel.capture(&ctx, &mut events, |_, _| false));
    assert!(events.is_empty());
    until(&mut panel.session, |session| session.state.finished());
    assert!(visible(&panel.session).contains("KEYS="));
    assert!(visible(&panel.session).contains("typing"));
    assert_eq!(
        input::key_bytes(egui::Key::C, egui::Modifiers::CTRL, TermMode::empty()),
        Some(vec![3])
    );
    assert_eq!(
        input::key_bytes(
            egui::Key::ArrowUp,
            egui::Modifiers::NONE,
            TermMode::APP_CURSOR
        ),
        Some(b"\x1bOA".to_vec())
    );
    assert_eq!(
        input::paste("one\r\ntwo\x1b", TermMode::BRACKETED_PASTE),
        b"\x1b[200~one\ntwo\x1b[201~"
    );
}

#[test]
#[ignore = "requires disposable OpenSSH; run PAWCOMMANDER_TEST_SFTP_TERMINAL=embedded bash scripts/test-sftp.sh"]
fn openssh_terminal_embedded_propagates_pty_size_and_folder() {
    let root = PathBuf::from(std::env::var_os("PAWCOMMANDER_TEST_SFTP_ROOT").unwrap());
    let endpoint = Endpoint {
        host: std::env::var("PAWCOMMANDER_TEST_SFTP_HOST").unwrap(),
        port: std::env::var("PAWCOMMANDER_TEST_SFTP_PORT")
            .unwrap()
            .parse()
            .unwrap(),
        user: std::env::var("PAWCOMMANDER_TEST_SFTP_USER").unwrap(),
        key: Some(PathBuf::from(
            std::env::var_os("PAWCOMMANDER_TEST_TERMINAL_KEY").unwrap(),
        )),
        jump: None,
    };
    let folder = root.join("Embedded SSH reports ' draft & é");
    std::fs::create_dir(&folder).unwrap();
    let target = folder.clone();
    let known_hosts = root.join("known_hosts");
    let ctx = egui::Context::default();
    let mut terminal = Session::start_with(&ctx, move || {
        let ssh = crate::terminal::macos::ssh_command(&endpoint, &target)?;
        let mut command = Command::new(ssh.get_program());
        command
            .args([
                "-F",
                "/dev/null",
                "-o",
                "IdentityAgent=none",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "GlobalKnownHostsFile=/dev/null",
                "-o",
                "ConnectTimeout=4",
                "-o",
            ])
            .arg(format!("UserKnownHostsFile={}", known_hosts.display()))
            .args(ssh.get_args());
        for (name, value) in ssh.get_envs() {
            command.env(name, value.unwrap());
        }
        Ok(command)
    })
    .unwrap();
    // Wait for the remote shell to accept input before sending the resize probe.
    assert!(terminal.input(b"if test -t 0; then printf 'PAW-%s\\n' REMOTE; fi\r"));
    until(&mut terminal, |terminal| {
        visible(terminal).contains("PAW-REMOTE")
    });
    terminal.resize(session::Size {
        rows: 31,
        columns: 92,
        ..session::Size::default()
    });
    let folder = folder.to_str().unwrap().replace('\'', "'\\''");
    let script = format!("test . -ef '{folder}' || exit 4; printf 'PAW-%s\\n' FOLDER; i=0; until test \"$(/bin/stty size)\" = '31 92'; do i=$((i+1)); test \"$i\" -lt 20 || exit 5; /bin/sleep .05; done; printf 'PAW-%s\\n' SIZE; exit 0\r");
    assert!(terminal.input(script.as_bytes()));
    until(&mut terminal, |terminal| terminal.state.finished());
    let screen = visible(&terminal);
    assert!(
        matches!(terminal.state, State::Exited(status) if status.success()),
        "{screen}"
    );
    assert!(
        screen.contains("PAW-FOLDER") && screen.contains("PAW-SIZE"),
        "{screen}"
    );
}
