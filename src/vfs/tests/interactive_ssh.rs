use super::*;
use crate::file_error::{ConnectionError, Failure, HostKeyError, PinError, SshStage};
use crate::i18n::connection_result_error;
use crate::vfs::sftp::TrustFiles;
use crate::vfs::{
    AuthRequest, ConnectRequest, ConnectedLocation, ConnectionControl, ConnectionPrompt,
    HostKeyRequest, HostKeyRole, InteractiveAuth,
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
    result: mpsc::Receiver<Result<ConnectedLocation, TransferError>>,
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
                jump: None,
            },
            path: ".".into(),
            password: Zeroizing::new(String::new()),
            passphrase: Zeroizing::new(String::new()),
            #[cfg(target_os = "macos")]
            jump_passphrase: Zeroizing::new(String::new()),
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

    fn finish(&self) -> Result<ConnectedLocation, TransferError> {
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
    assert_eq!(offered.role, HostKeyRole::Server);
    assert_eq!(offered.key.fingerprint, fingerprints[0]);
    assert!(unverified.control.cancel());
    drop(offered);
    assert!(matches!(unverified.finish(), Err(TransferError::Cancelled)));
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
    let error = cancelled.finish().err().unwrap();
    assert!(matches!(error, TransferError::Cancelled));
    assert_eq!(
        connection_result_error(&error),
        crate::i18n::text!("Подключение отменено")
    );

    let expired = fixture.start("timeout-fixture", true, Duration::from_millis(180));
    let _prompt = expired.prompt();
    let error = expired.finish().err().unwrap();
    assert!(matches!(
        &error,
        TransferError::Failed(Failure::Connection(cause))
            if matches!(cause.as_ref(), ConnectionError::InteractiveTimeout)
    ));
    assert_eq!(
        connection_result_error(&error),
        crate::i18n::text!("Время интерактивной SSH-аутентификации истекло")
    );

    let rejected = fixture.start("reject-fixture", true, timeout);
    answer(
        rejected.prompt(),
        &["fixture-rejected-answer", "fixture-label"],
    );
    let error = rejected.finish().err().unwrap();
    assert!(
        matches!(
            &error,
            TransferError::Failed(Failure::Connection(cause))
                if matches!(cause.as_ref(), ConnectionError::Ssh {
                    stage: SshStage::InteractiveAuthentication, source,
                } if source.code() == ssh2::ErrorCode::Session(libssh2_sys::LIBSSH2_ERROR_AUTHENTICATION_FAILED))
        ),
        "{error:?}"
    );
    let message = connection_result_error(&error);
    assert!(message.starts_with(crate::i18n::text!(
        "Сервер не подтвердил интерактивную аутентификацию"
    )));
    assert!(!message.contains("fixture-rejected-answer"));
    assert!(!fixture.root.join("legacy-pins").exists());
    assert!(!fixture.root.join("absent_hosts").exists());

    let mut records = fs::read_dir(&pins).unwrap();
    let record = records.next().unwrap().unwrap().path();
    assert!(records.next().is_none());
    let before = fs::read(&record).unwrap();
    let changed = fixture.start("changed-host", false, timeout);
    let error = changed.finish().err().unwrap();
    let TransferError::Failed(Failure::Connection(cause)) = &error else {
        panic!("lost connection cause: {error:?}");
    };
    let ConnectionError::HostKey(HostKeyError::Pin {
        path,
        cause: PinError::Changed { saved, received },
    }) = cause.as_ref()
    else {
        panic!("lost key mismatch details: {cause:?}");
    };
    assert_eq!(path, &record);
    assert_eq!(saved, fingerprints[0]);
    assert_eq!(received, fingerprints[1]);
    let message = connection_result_error(&error);
    assert!(message.starts_with(crate::i18n::text!("Ключ сервера SSH изменился.")));
    assert!(message.contains(fingerprints[0]) && message.contains(fingerprints[1]));
    assert!(changed.prompts.try_recv().is_err());
    assert_eq!(fs::read(&record).unwrap(), before);
    assert!(!sessions()
        .lock()
        .unwrap()
        .keys()
        .any(|endpoint| endpoint.port == fixture.port));
}
