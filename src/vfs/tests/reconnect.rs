use super::*;
use crate::result_history::Outcome;
use crate::vfs::RetryTransfer;
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

fn interrupt_rename(
    fixture: &Path,
    source: &Location,
    destination: &Location,
    kind: TransferKind,
    replace: bool,
    expected_marker: &str,
) -> (RetryTransfer, Arc<ssh2::Session>) {
    drop(remote(destination).unwrap());
    let endpoint = destination.sftp_endpoint().unwrap();
    let session = sessions().lock().unwrap()[endpoint].clone();
    // SAFETY: the cached session is held until we have duplicated its socket.
    let fd = unsafe { BorrowedFd::borrow_raw(session.as_raw_fd()) };
    let socket = TcpStream::from(fd.try_clone_to_owned().unwrap());
    let name = source.path().file_name().unwrap().to_str().unwrap();
    let marker = fixture.join(format!("held-{name}"));
    let disconnect = thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(12);
        loop {
            if let Ok(stage) = fs::read_to_string(&marker) {
                socket.shutdown(Shutdown::Both).unwrap();
                return stage;
            }
            assert!(Instant::now() < until, "SFTP rename was never held");
            thread::sleep(Duration::from_millis(10));
        }
    });
    let mut progress = Progress::default();
    progress.retry = Some(RetryTransfer::new(kind, destination.clone()));
    let operation = transfer_with_conflicts(
        std::slice::from_ref(source),
        destination,
        kind,
        &mut progress,
        |_| {
            assert!(replace, "unexpected replacement conflict");
            Ok(Answer {
                decision: Decision::Replace,
                apply_to_group: false,
            })
        },
        |_, _| panic!("a move without its reply cannot report completion"),
    );
    assert_eq!(disconnect.join().unwrap(), expected_marker);
    assert!(operation.is_err(), "the client received the withheld reply");
    assert_eq!(
        progress.results.root_outcomes(),
        [(source.clone(), Outcome::PublicationUnconfirmed)]
    );
    let retry = progress
        .retry
        .take()
        .unwrap()
        .unfinished(&progress.results)
        .unwrap();
    assert!(retry.needs_inspection());
    (retry, session)
}

#[test]
#[ignore = "requires the disposable reply-drop subsystem from scripts/test-sftp.sh"]
fn sftp_reconnect_checks_lost_publication_replies() {
    let fixture = PathBuf::from(std::env::var_os("PAWCOMMANDER_TEST_SFTP_ROOT").unwrap());
    let endpoint = Endpoint {
        host: std::env::var("PAWCOMMANDER_TEST_SFTP_HOST").unwrap(),
        port: std::env::var("PAWCOMMANDER_TEST_SFTP_PORT")
            .unwrap()
            .parse()
            .unwrap(),
        user: std::env::var("PAWCOMMANDER_TEST_SFTP_USER").unwrap(),
        key: None,
    };
    let remote_root = Location::Sftp {
        endpoint: endpoint.clone(),
        path: fixture.join("output"),
    };
    fs::create_dir(remote_root.path()).unwrap();
    const CONTENTS: &[u8] = b"fixture data saved over SFTP";
    for (name, expected_marker, replace) in [
        ("lost-before.bin", "before", false),
        ("lost-after.bin", "after", false),
        ("lost-replacement.bin", "after", true),
        ("lost-conflict.bin", "after", false),
    ] {
        let source = Location::Local(fixture.join(name));
        let target = remote_root.join(name);
        fs::write(source.path(), CONTENTS).unwrap();
        if replace {
            fs::write(target.path(), b"older destination").unwrap();
        }
        let (retry, connection) = interrupt_rename(
            &fixture,
            &source,
            &remote_root,
            TransferKind::Copy,
            replace,
            expected_marker,
        );
        if expected_marker == "after" {
            assert_eq!(fs::read(target.path()).unwrap(), CONTENTS);
        } else {
            assert!(!target.path().exists());
        }
        if name == "lost-conflict.bin" {
            fs::write(target.path(), b"independent edit").unwrap();
        }
        let mut review = Progress::default();
        let (ready, pending) = retry.inspect(&mut review).unwrap();
        assert!(!Arc::ptr_eq(
            &connection,
            &sessions().lock().unwrap()[&endpoint]
        ));
        assert_eq!(fs::read(source.path()).unwrap(), CONTENTS);
        let row = &review.results.items[0];
        assert_eq!(row.target.as_deref(), Some(target.display().as_str()));
        match name {
            "lost-before.bin" => {
                assert_eq!(row.outcome, Outcome::RetryReady);
                assert!(pending.is_none());
                let ready = ready.unwrap();
                assert!(!ready.needs_inspection());
                assert!(!target.path().exists(), "inspection must be read-only");
                let mut retry_progress = Progress::default();
                retry_progress.retry = Some(ready.clone());
                fs::write(target.path(), b"changed after inspection").unwrap();
                let result = transfer_with_conflicts(
                    &ready.sources(),
                    &remote_root,
                    TransferKind::Copy,
                    &mut retry_progress,
                    |_| panic!("refuse the changed target before asking about a conflict"),
                    |_, _| {},
                );
                assert!(result.is_err());
                assert_eq!(
                    fs::read(target.path()).unwrap(),
                    b"changed after inspection"
                );
                fs::remove_file(target.path()).unwrap();
                let mut retry_progress = Progress::default();
                retry_progress.retry = Some(ready);
                let result = transfer_with_conflicts(
                    &[source],
                    &remote_root,
                    TransferKind::Copy,
                    &mut retry_progress,
                    |_| panic!("target remains absent"),
                    |_, _| {},
                )
                .unwrap();
                assert_eq!(result.completed, 1);
                assert_eq!(fs::read(target.path()).unwrap(), CONTENTS);
            }
            "lost-conflict.bin" => {
                assert_eq!(row.outcome, Outcome::PublicationUnconfirmed);
                assert!(ready.is_none() && pending.unwrap().needs_inspection());
                assert_eq!(fs::read(target.path()).unwrap(), b"independent edit");
            }
            _ => {
                assert_eq!(row.outcome, Outcome::CompletedWithWarning);
                assert!(ready.is_none() && pending.is_none());
                assert_eq!(fs::read(target.path()).unwrap(), CONTENTS);
                if replace {
                    let retained: Vec<_> = fs::read_dir(remote_root.path())
                        .unwrap()
                        .filter_map(|entry| fs::read(entry.unwrap().path().join("previous")).ok())
                        .collect();
                    assert_eq!(retained, [b"older destination"]);
                }
            }
        }
    }
    let source = remote_root.with_path(fixture.join("lost-move-dir"));
    fs::create_dir_all(source.join("nested").path()).unwrap();
    fs::write(source.join("nested/data").path(), CONTENTS).unwrap();
    let (retry, connection) = interrupt_rename(
        &fixture,
        &source,
        &remote_root,
        TransferKind::Move,
        false,
        "after",
    );
    let mut review = Progress::default();
    let (ready, pending) = retry.inspect(&mut review).unwrap();
    assert!(!Arc::ptr_eq(
        &connection,
        &sessions().lock().unwrap()[&endpoint]
    ));
    assert!(ready.is_none() && pending.is_none());
    assert_eq!(
        review.results.items[0].outcome,
        Outcome::PublicationUnconfirmed
    );
    assert!(review.results.items[0].reason.as_ref().unwrap().contains(
        crate::i18n::text("Источник отсутствует. Состав и метаданные назначения совпадают с записанными перед переносом.")
    ));
    assert!(!source.path().exists());
    assert_eq!(
        fs::read(remote_root.join("lost-move-dir/nested/data").path()).unwrap(),
        CONTENTS
    );
    sessions().lock().unwrap().remove(&endpoint);
}
