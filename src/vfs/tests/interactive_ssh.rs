use super::*;
use crate::vfs::{
    AuthRequest, ConnectRequest, ConnectedLocation, ConnectionControl, InteractiveAuth,
};
use std::{sync::mpsc, thread};
use zeroize::Zeroizing;

struct Fixture {
    root: PathBuf,
    port: u16,
}

struct Attempt {
    user: String,
    prompts: mpsc::Receiver<AuthRequest>,
    control: ConnectionControl,
    result: mpsc::Receiver<Result<ConnectedLocation, String>>,
}

impl Fixture {
    fn start(&self, user: &str, trust: bool, timeout: Duration) -> Attempt {
        let (mut auth, prompts) = InteractiveAuth::new(|| {});
        auth.set_timeout(timeout);
        let control = auth.control();
        let request = ConnectRequest {
            endpoint: Endpoint {
                host: "127.0.0.1".into(),
                port: self.port,
                user: user.into(),
                key: (user == "mfa-fixture").then(|| self.root.join("client")),
            },
            path: ".".into(),
            password: Zeroizing::new(String::new()),
            passphrase: Zeroizing::new(String::new()),
            use_saved: false,
            save_key: false,
        };
        let known_hosts = self
            .root
            .join(if trust { "known_hosts" } else { "empty_hosts" });
        let (done, result) = mpsc::channel();
        thread::spawn(move || {
            let connected = super::super::sftp::connect_location_at(request, auth, &known_hosts);
            let _ = done.send(connected);
        });
        Attempt {
            user: user.into(),
            control,
            prompts,
            result,
        }
    }
}

impl Attempt {
    fn prompt(&self) -> AuthRequest {
        self.prompts
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|error| {
                let failure = self.result.try_recv().ok().and_then(Result::err);
                panic!(
                    "Missing prompt for {} ({error}), terminal error {failure:?}",
                    self.user
                );
            })
    }

    fn finish(&self) -> Result<ConnectedLocation, String> {
        self.result.recv_timeout(Duration::from_secs(5)).unwrap()
    }
}

fn answer(request: AuthRequest, responses: &[&str]) {
    request.answer(
        responses
            .iter()
            .map(|value| Zeroizing::new((*value).into()))
            .collect(),
    );
}

#[test]
#[ignore = "requires the loopback keyboard-interactive server from scripts/test-ssh-interactive.sh"]
fn ssh_interactive_round_trip() {
    let fixture = Fixture {
        root: std::env::var_os("PAWCOMMANDER_TEST_INTERACTIVE_ROOT")
            .unwrap()
            .into(),
        port: std::env::var("PAWCOMMANDER_TEST_INTERACTIVE_PORT")
            .unwrap()
            .parse()
            .unwrap(),
    };
    fs::write(fixture.root.join("empty_hosts"), "").unwrap();
    let timeout = Duration::from_secs(3);
    let unverified = fixture.start("unknown-host", false, timeout);
    assert!(unverified
        .finish()
        .err()
        .unwrap()
        .contains("не подтверждён"));
    assert!(unverified.prompts.try_recv().is_err());

    for user in ["mfa-fixture", "interactive-fixture"] {
        let attempt = fixture.start(user, true, timeout);
        let prompt = attempt.prompt();
        assert_eq!(prompt.instructions, "First round\nTwo answers");
        assert_eq!(prompt.questions.len(), 2);
        assert_eq!(prompt.questions[0].label, "One-time code");
        assert!(!prompt.questions[0].echo && prompt.questions[1].echo);
        answer(prompt, &["fixture-code", "fixture-label"]);
        let prompt = attempt.prompt();
        assert_eq!(prompt.questions[0].label, "Confirmation");
        answer(prompt, &["fixture-confirmation"]);
        let connected = attempt.finish().unwrap();
        assert_eq!(connected.location.path(), Path::new("/fixture"));
        assert!(
            !attempt.control.cancel(),
            "published connection should stay active"
        );
        assert!(remote(&connected.location)
            .unwrap()
            .stat(Path::new("/fixture"))
            .unwrap()
            .is_dir());
        sessions()
            .lock()
            .unwrap()
            .remove(connected.location.sftp_endpoint().unwrap());
    }

    let cancelled = fixture.start("cancel-fixture", true, timeout);
    let _prompt = cancelled.prompt();
    assert!(cancelled.control.cancel());
    assert_eq!(
        cancelled.finish().err().unwrap(),
        crate::i18n::text("Подключение отменено")
    );

    let expired = fixture.start("timeout-fixture", true, Duration::from_millis(180));
    let _prompt = expired.prompt();
    assert_eq!(
        expired.finish().err().unwrap(),
        crate::i18n::text("Время интерактивной SSH-аутентификации истекло")
    );

    let rejected = fixture.start("reject-fixture", true, timeout);
    answer(
        rejected.prompt(),
        &["fixture-rejected-answer", "fixture-label"],
    );
    let error = rejected.finish().err().unwrap();
    assert_eq!(
        error,
        crate::i18n::text("Сервер не подтвердил интерактивную аутентификацию")
    );
    assert!(!error.contains("fixture-rejected-answer"));
    assert!(!sessions()
        .lock()
        .unwrap()
        .keys()
        .any(|endpoint| endpoint.port == fixture.port));
}
