use super::*;
use crate::vfs::sftp::TrustFiles;
use crate::vfs::{
    AuthRequest, ConnectRequest, ConnectedLocation, ConnectionControl, ConnectionPrompt,
    HostKeyRequest, InteractiveAuth,
};
use std::{sync::mpsc, thread};
use zeroize::Zeroizing;

struct Fixture {
    root: PathBuf,
    port: u16,
}

struct Attempt {
    user: String,
    prompts: mpsc::Receiver<ConnectionPrompt>,
    control: ConnectionControl,
    result: mpsc::Receiver<Result<ConnectedLocation, String>>,
}

impl Fixture {
    fn start(&self, user: &str, use_known_hosts: bool, timeout: Duration) -> Attempt {
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
        let trust = if use_known_hosts {
            TrustFiles::new(
                self.root.join("known_hosts"),
                Some(self.root.join("legacy-pins")),
            )
        } else {
            TrustFiles::new(self.root.join("absent_hosts"), Some(self.root.join("pins")))
        };
        let (done, result) = mpsc::channel();
        thread::spawn(move || {
            let connected = super::super::sftp::connect_location_at(request, auth, &trust);
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
    fn request(&self) -> ConnectionPrompt {
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

    fn prompt(&self) -> AuthRequest {
        let ConnectionPrompt::Authentication(prompt) = self.request() else {
            panic!("Expected an authentication prompt for {}", self.user);
        };
        prompt
    }

    fn host_key(&self) -> HostKeyRequest {
        let ConnectionPrompt::HostKey(prompt) = self.request() else {
            panic!("Expected a host key prompt for {}", self.user);
        };
        prompt
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
    let fingerprints = fs::read_to_string(fixture.root.join("fingerprints")).unwrap();
    let fingerprints: Vec<_> = fingerprints.lines().collect();
    let pins = fixture.root.join("pins");
    let timeout = Duration::from_secs(3);
    let unverified = fixture.start("unknown-host", false, timeout);
    let offered = unverified.host_key();
    assert_eq!(offered.key.fingerprint, fingerprints[0]);
    assert!(unverified.control.cancel());
    drop(offered);
    assert_eq!(
        unverified.finish().err().unwrap(),
        crate::i18n::text("Подключение отменено")
    );
    assert!(unverified.prompts.try_recv().is_err());
    assert!(!pins.exists());

    for user in ["mfa-fixture", "interactive-fixture"] {
        let attempt = fixture.start(user, false, timeout);
        if user == "mfa-fixture" {
            let host_key = attempt.host_key();
            assert_eq!(
                host_key.key.address,
                format!("[127.0.0.1]:{}", fixture.port)
            );
            assert_eq!(host_key.key.algorithm, "ssh-ed25519");
            assert_eq!(host_key.key.fingerprint, fingerprints[0]);
            host_key.approve();
        }
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
    assert!(!fixture.root.join("legacy-pins").exists());
    assert!(!fixture.root.join("absent_hosts").exists());

    let mut records = fs::read_dir(&pins).unwrap();
    let record = records.next().unwrap().unwrap().path();
    assert!(records.next().is_none());
    let before = fs::read(&record).unwrap();
    let changed = fixture.start("changed-host", false, timeout);
    let error = changed.finish().err().unwrap();
    assert!(error.starts_with(crate::i18n::text("Ключ сервера SSH изменился.")));
    assert!(error.contains(fingerprints[0]) && error.contains(fingerprints[1]));
    assert!(changed.prompts.try_recv().is_err());
    assert_eq!(fs::read(&record).unwrap(), before);
    assert!(!sessions()
        .lock()
        .unwrap()
        .keys()
        .any(|endpoint| endpoint.port == fixture.port));
}
