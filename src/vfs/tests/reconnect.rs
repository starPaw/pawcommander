use super::*;
use crate::result_history::Outcome;
use crate::vfs::RetryTransfer;
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

fn disconnect_at_request(
    fixture: &Path,
    source: &Location,
    session: &ssh2::Session,
    wait: Duration,
) -> thread::JoinHandle<String> {
    // SAFETY: the cached session is held until we have duplicated its socket.
    let fd = unsafe { BorrowedFd::borrow_raw(session.as_raw_fd()) };
    let socket = TcpStream::from(fd.try_clone_to_owned().unwrap());
    let name = source.path().file_name().unwrap().to_str().unwrap();
    let marker = fixture.join(format!("held-{name}"));
    thread::spawn(move || {
        let until = Instant::now() + wait;
        loop {
            if let Ok(stage) = fs::read_to_string(&marker) {
                socket.shutdown(Shutdown::Both).unwrap();
                return stage;
            }
            if Instant::now() >= until {
                let _ = socket.shutdown(Shutdown::Both);
                panic!("SFTP request never reached {}", marker.display());
            }
            thread::sleep(Duration::from_millis(10));
        }
    })
}

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
    let disconnect = disconnect_at_request(fixture, source, &session, Duration::from_secs(12));
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
    let failure = operation.expect_err("the client received the withheld reply");
    check_rename_causes(&failure.error, source, destination, kind, replace);
    assert_eq!(failure.summary.completed, 0);
    assert_eq!(failure.summary.remaining, std::slice::from_ref(source));
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

fn check_rename_causes(
    error: &TransferError,
    source: &Location,
    destination: &Location,
    kind: TransferKind,
    replaced: bool,
) {
    use crate::file_error::{Failure, MutationError, PublicationError, RecoveryNotice};

    let target = destination.join(source.name());
    let (cause, from) = if kind == TransferKind::Copy {
        let TransferError::PublicationUnconfirmed(Failure::Publication(retained)) = error else {
            panic!("expected the retained staging path: {error:?}");
        };
        let PublicationError::Retained {
            staging,
            notice: RecoveryNotice::InspectStaging,
            cause,
        } = retained.as_ref()
        else {
            panic!("expected the retained staging path: {retained:?}");
        };
        let primary = if replaced {
            let TransferError::PublicationUnconfirmed(Failure::Publication(rollback)) = cause
            else {
                panic!("expected a retained backup with both errors: {cause:?}");
            };
            let PublicationError::Rollback {
                target: attempted,
                backup,
                cause,
                restore,
            } = rollback.as_ref()
            else {
                panic!("expected both errors from the two rename calls: {rollback:?}");
            };
            assert_eq!(attempted, &target);
            assert_eq!(backup, &staging.join("previous"));
            assert!(
                matches!(
                    restore, TransferError::PublicationUnconfirmed(Failure::Mutation(reason))
                        if matches!(reason.as_ref(), MutationError::RenameSftp { from, to, source }
                            if from == backup && to == &target && matches!(source.code(), ssh2::ErrorCode::SFTP(_)))
                ),
                "the restored target was already occupied: {restore:?}"
            );
            cause
        } else {
            cause
        };
        (primary, staging.join("payload"))
    } else {
        (error, source.clone())
    };
    assert!(
        matches!(
            cause, TransferError::PublicationUnconfirmed(Failure::Mutation(reason))
                if matches!(reason.as_ref(), MutationError::RenameSftp { from: actual, to, source }
                    if actual == &from && to == &target && matches!(source.code(), ssh2::ErrorCode::Session(_)))
        ),
        "the original rename transport error and paths were lost: {cause:?}"
    );
}

#[cfg(target_os = "macos")]
fn check_lost_source_hold(fixture: &Path, remote_root: &Location) {
    use crate::vfs::resume_copy::{resume, scan};
    use std::os::unix::fs::MetadataExt;

    let input = remote_root.join("held-source-input");
    let destination: Location = fixture.join("held-source-output").into();
    let recovery = fixture.join("held-source-recovery");
    let source = input.join("lost-held-after");
    let target = destination.join("lost-held-after");
    let bytes = b"published folder contents";
    fs::create_dir_all(source.join("nested/empty").path()).unwrap();
    fs::write(source.join("nested/data").path(), bytes).unwrap();
    fs::create_dir(destination.path()).unwrap();
    drop(remote(&input).unwrap());
    let endpoint = input.sftp_endpoint().unwrap();
    let session = sessions().lock().unwrap()[endpoint].clone();
    let disconnected = disconnect_at_request(fixture, &source, &session, Duration::from_secs(45));
    let (mut progress, _) = Progress::tracked_transfer();
    progress.resume_directory = Some(recovery.clone());
    let interrupted = transfer_with_conflicts(
        std::slice::from_ref(&source),
        &destination,
        TransferKind::Move,
        &mut progress,
        |_| panic!("unexpected conflict"),
        |_, _| panic!("source removal was never confirmed"),
    );
    assert_eq!(disconnected.join().unwrap(), "after");
    assert!(interrupted.is_err());
    assert!(!source.path().exists());
    let held = fs::read_dir(input.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("source");
    assert_eq!(fs::read(held.join("nested/data")).unwrap(), bytes);
    assert_eq!(fs::read(target.join("nested/data").path()).unwrap(), bytes);
    let before = fs::metadata(target.path()).unwrap();

    let (offers, errors) = scan(&recovery);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(offers.len(), 1);
    assert!(offers[0].moving);
    let (mut progress, mut view) = Progress::tracked_transfer();
    resume(&recovery, &offers[0].id, &mut progress).unwrap();
    assert_eq!(view.snapshot().total_written, 0);
    assert!(!Arc::ptr_eq(
        &session,
        &sessions().lock().unwrap()[endpoint]
    ));
    assert_eq!(before.ino(), fs::metadata(target.path()).unwrap().ino());
    assert_eq!(fs::read(target.join("nested/data").path()).unwrap(), bytes);
    assert!(target.join("nested/empty").path().is_dir());
    assert!(fs::read_dir(input.path()).unwrap().next().is_none());
    assert!(scan(&recovery).0.is_empty());
    assert_eq!(progress.results.items.len(), 4);
    assert!(progress
        .results
        .items
        .iter()
        .all(|row| row.outcome == Outcome::Completed));
    println!("Resumed after the server moved the source into staging and lost its reply; wrote no file data");
}

fn check_lost_deletion_reply(fixture: &Path, remote_root: &Location) {
    use crate::file_error::{Failure, MutationError, SftpRemovalAction};

    let first = remote_root.join("removed-with-ack");
    let folder = remote_root.join("partly-removed");
    let child = folder.join("lost-remove.bin");
    let later = remote_root.join("keep-after-lost-reply");
    fs::create_dir(folder.path()).unwrap();
    fs::write(first.path(), b"acknowledged").unwrap();
    fs::write(child.path(), b"no acknowledgement").unwrap();
    fs::write(later.path(), b"untouched").unwrap();

    drop(remote(remote_root).unwrap());
    let session = sessions().lock().unwrap()[remote_root.sftp_endpoint().unwrap()].clone();
    let disconnect = disconnect_at_request(fixture, &child, &session, Duration::from_secs(15));
    let mut progress = Progress::default();
    let failure = trash_entries_with_progress(
        &[first.clone(), folder.clone(), later.clone()],
        &mut progress,
    )
    .unwrap_err();
    assert_eq!(disconnect.join().unwrap(), "after");
    assert!(
        matches!(
            &failure.error,
            TransferError::PublicationUnconfirmed(Failure::Mutation(reason))
                if matches!(reason.as_ref(), MutationError::RemoveSftp {
                    location, action: SftpRemovalAction::RemoveFile, source
                } if location == &child && matches!(source.code(), ssh2::ErrorCode::Session(_)))
        ),
        "wrong failure after lost deletion reply: {failure:?}"
    );
    assert_eq!(failure.completed, 1);
    assert!(failure.partial_remote);
    assert_eq!(
        progress
            .results
            .items
            .iter()
            .map(|row| row.outcome)
            .collect::<Vec<_>>(),
        [
            Outcome::Completed,
            Outcome::PublicationUnconfirmed,
            Outcome::NotStarted,
            Outcome::PublicationUnconfirmed,
        ]
    );
    assert_eq!(progress.results.items[3].source, child.display());
    assert!(!progress.results.items[1].descendants_enumerated);
    assert!(!first.path().exists());
    assert!(!child.path().exists());
    assert!(folder.path().is_dir());
    assert_eq!(fs::read(later.path()).unwrap(), b"untouched");
    fs::remove_file(later.path()).unwrap();
    fs::remove_dir(folder.path()).unwrap();
    println!(
        "A lost deletion reply left the selected folder unconfirmed and preserved the later item"
    );
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
        jump: None,
    };
    let remote_root = Location::Sftp {
        endpoint: endpoint.clone(),
        path: fixture.join("output"),
    };
    fs::create_dir(remote_root.path()).unwrap();
    check_lost_deletion_reply(&fixture, &remote_root);
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
        crate::i18n::text!("Источник отсутствует. Состав и метаданные назначения совпадают с записанными перед переносом.")
    ));
    assert!(!source.path().exists());
    assert_eq!(
        fs::read(remote_root.join("lost-move-dir/nested/data").path()).unwrap(),
        CONTENTS
    );
    #[cfg(target_os = "macos")]
    {
        check_lost_source_hold(&fixture, &remote_root);
        crate::vfs::resume_copy::tests::check_sftp(&fixture, &remote_root);
    }
    sessions().lock().unwrap().remove(&endpoint);
}
