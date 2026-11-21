use super::{remote, sessions, Endpoint, Path, PathBuf};
use crate::file_error::{ConnectionError, Failure, HostKeyError};
use crate::i18n::connection_result_error;
use crate::operations::TransferError;
use crate::vfs::{
    sftp::{connect_location_at, TrustFiles},
    ConnectRequest, ConnectedLocation, ConnectionControl, ConnectionPrompt, HostKeyRole,
    InteractiveAuth, JumpHost,
};
use std::{
    fs,
    io::{Read, Write},
    process::Command,
    sync::mpsc,
    thread,
    time::Duration,
};
use zeroize::Zeroizing;

const WAIT: Duration = Duration::from_secs(25);
const JUMP_PHRASE: &str = "jump-test-passphrase";

struct Connection {
    prompts: mpsc::Receiver<ConnectionPrompt>,
    result: mpsc::Receiver<Result<ConnectedLocation, TransferError>>,
    control: ConnectionControl,
}

impl Connection {
    fn launch(endpoint: &Endpoint, root: &Path, trust: TrustFiles, jump_phrase: &str) -> Self {
        let request = ConnectRequest {
            endpoint: endpoint.clone(),
            path: root.to_str().unwrap().into(),
            password: Zeroizing::default(),
            passphrase: Zeroizing::new("test-passphrase".into()),
            jump_passphrase: Zeroizing::new(jump_phrase.into()),
            use_saved: false,
            save_key: false,
        };
        let (auth, prompts) = InteractiveAuth::new(|| {});
        let control = auth.control();
        let (send, result) = mpsc::channel();
        thread::spawn(move || {
            let _ = send.send(connect_location_at(request, auth, &trust));
        });
        Self {
            prompts,
            result,
            control,
        }
    }

    fn key(&self, port: u16, expected: &str, role: HostKeyRole) -> crate::vfs::HostKeyRequest {
        let ConnectionPrompt::HostKey(prompt) = self.prompts.recv_timeout(WAIT).unwrap() else {
            panic!("Expected a host key prompt");
        };
        assert_eq!(prompt.key.address, format!("[127.0.0.1]:{port}"));
        assert_eq!(prompt.key.fingerprint, expected);
        assert_eq!(prompt.role, role);
        prompt
    }

    fn finish(self) -> Result<ConnectedLocation, TransferError> {
        let result = self.result.recv_timeout(WAIT).unwrap();
        assert!(
            self.prompts.try_recv().is_err(),
            "Unexpected extra authentication prompt"
        );
        result
    }
}

fn fingerprint(key: &Path) -> String {
    let output = Command::new("ssh-keygen")
        .args(["-l", "-E", "sha256", "-f"])
        .arg(key)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned()
}

#[test]
#[ignore = "requires two disposable OpenSSH servers; run PAWCOMMANDER_TEST_SFTP_PROXYJUMP=1 bash scripts/test-sftp.sh"]
fn openssh_proxy_jump_verifies_both_hosts_and_forwards_sftp() {
    let root = PathBuf::from(std::env::var_os("PAWCOMMANDER_TEST_SFTP_ROOT").unwrap());
    let port = std::env::var("PAWCOMMANDER_TEST_SFTP_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let jump_port = std::env::var("PAWCOMMANDER_TEST_SFTP_JUMP_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let user = std::env::var("PAWCOMMANDER_TEST_SFTP_USER").unwrap();
    let endpoint = Endpoint {
        host: "127.0.0.1".into(),
        port,
        user: user.clone(),
        key: Some(root.join("client")),
        jump: Some(Box::new(JumpHost {
            host: "127.0.0.1".into(),
            port: jump_port,
            user,
            key: Some(root.join("jump-client")),
        })),
    };
    let jump_hash = fingerprint(&root.join("jump-host.pub"));
    let target_hash = fingerprint(&root.join("host.pub"));
    assert_ne!(jump_hash, target_hash);
    let fresh_trust = || TrustFiles::new(root.join("missing"), Some(root.join("pins")));
    let known_trust = || TrustFiles::new(root.join("known_hosts"), None);

    let cancelled = Connection::launch(&endpoint, &root, fresh_trust(), JUMP_PHRASE);
    cancelled
        .key(jump_port, &jump_hash, HostKeyRole::Jump)
        .approve();
    let _target = cancelled.key(port, &target_hash, HostKeyRole::Target);
    assert!(cancelled.control.cancel());
    assert!(matches!(cancelled.finish(), Err(TransferError::Cancelled)));
    assert!(!sessions().lock().unwrap().contains_key(&endpoint));

    let accepted = Connection::launch(&endpoint, &root, fresh_trust(), JUMP_PHRASE);
    accepted
        .key(port, &target_hash, HostKeyRole::Target)
        .approve();
    let location = accepted.finish().unwrap().location;
    assert_eq!(location.sftp_endpoint(), Some(&endpoint));
    let channel = remote(&location).unwrap();
    let file = location.path().join("proxy-jump-payload");
    let payload: Vec<u8> = (0..5 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    channel.create(&file).unwrap().write_all(&payload).unwrap();
    assert!(fs::read(&file).unwrap() == payload);
    let mut received = Vec::new();
    channel
        .open(&file)
        .unwrap()
        .read_to_end(&mut received)
        .unwrap();
    assert!(received == payload);
    drop(channel);
    drop(sessions().lock().unwrap().remove(&endpoint));
    let reconnected = Connection::launch(&endpoint, &root, known_trust(), JUMP_PHRASE);
    assert_eq!(reconnected.finish().unwrap().location, location);
    drop(sessions().lock().unwrap().remove(&endpoint));

    let denied = Connection::launch(&endpoint, &root, known_trust(), "wrong jump phrase");
    let error = denied
        .finish()
        .err()
        .expect("Jump host must reject the wrong phrase");
    let message = connection_result_error(&error);
    assert!(message.contains("Jump host"), "{message}");

    let denied_target = Endpoint {
        host: "localhost".into(),
        ..endpoint.clone()
    };
    let denied = Connection::launch(&denied_target, &root, known_trust(), JUMP_PHRASE);
    let error = denied
        .finish()
        .err()
        .expect("PermitOpen must reject localhost");
    let TransferError::Failed(Failure::Connection(route)) = &error else {
        panic!("lost connection cause: {error:?}");
    };
    let ConnectionError::Route {
        jump: true,
        cause: TransferError::Failed(Failure::Connection(forward)),
        ..
    } = route.as_ref()
    else {
        panic!("lost jump host context: {route:?}");
    };
    assert!(
        matches!(forward.as_ref(), ConnectionError::Forward { target, source }
        if *target == denied_target.label() && matches!(source.code(), ssh2::ErrorCode::Session(_)))
    );
    let error = connection_result_error(&error);
    assert!(
        error.contains("Jump host") && error.contains("Could not forward TCP"),
        "{error}"
    );

    let other_key = fs::read_to_string(root.join("other.pub")).unwrap();
    let jump_key = fs::read_to_string(root.join("jump-host.pub")).unwrap();
    let target_key = fs::read_to_string(root.join("host.pub")).unwrap();
    for (records, expected_jump, actual_hash) in [
        (
            format!("[127.0.0.1]:{jump_port} {other_key}[127.0.0.1]:{port} {target_key}"),
            true,
            &jump_hash,
        ),
        (
            format!("[127.0.0.1]:{jump_port} {jump_key}[127.0.0.1]:{port} {other_key}"),
            false,
            &target_hash,
        ),
    ] {
        let mismatched = root.join("mismatched");
        fs::write(&mismatched, records).unwrap();
        let trust = TrustFiles::new(mismatched.clone(), None);
        let result = Connection::launch(&endpoint, &root, trust, JUMP_PHRASE).finish();
        let error = result
            .err()
            .expect("Host verification must reject the mismatched key");
        let TransferError::Failed(Failure::Connection(route)) = &error else {
            panic!("lost connection cause: {error:?}");
        };
        let ConnectionError::Route {
            jump,
            cause: TransferError::Failed(Failure::Connection(cause)),
            ..
        } = route.as_ref()
        else {
            panic!("lost route context: {route:?}");
        };
        assert_eq!(*jump, expected_jump);
        assert!(
            matches!(cause.as_ref(), ConnectionError::HostKey(HostKeyError::KnownHostsChanged { path, received })
            if path == &mismatched && received == actual_hash)
        );
        let error = connection_result_error(&error);
        let expected_stage = if expected_jump {
            "Jump host"
        } else {
            "Target host"
        };
        assert!(
            error.contains(expected_stage) && error.contains(actual_hash),
            "{error}"
        );
    }
    assert!(!sessions().lock().unwrap().contains_key(&endpoint));
}
