use super::*;
#[cfg(target_os = "macos")]
use crate::vfs::cache_media_preview;

#[cfg(target_os = "macos")]
#[test]
fn key_import_failures_are_typed_before_opening_a_connection() {
    use crate::file_error::{AccessAction, AccessCause, ConnectionError, Failure};
    use crate::vfs::{
        sftp::{connect_location_at, TrustFiles},
        ConnectRequest, InteractiveAuth,
    };
    use std::error::Error;
    use zeroize::Zeroizing;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let endpoint = Endpoint {
        host: "127.0.0.1".into(),
        port: 49152,
        user: "fixture".into(),
        key: None,
        jump: None,
    };
    let absent = root.join("missing");
    let large = root.join("large");
    let invalid = root.join("invalid");
    let password = "fixture-account-password";
    let phrase = "fixture-key-phrase";
    let prefix = b"fixture-private-key-data";
    fs::write(&large, vec![0_u8; 64 * 1024 + 1]).unwrap();
    fs::write(&invalid, [prefix.as_slice(), &[0xff]].concat()).unwrap();
    let trust = TrustFiles::new(root.join("known_hosts"), None);
    let request = |key: Option<PathBuf>, password: &str| ConnectRequest {
        endpoint: Endpoint {
            key,
            ..endpoint.clone()
        },
        path: ".".into(),
        password: Zeroizing::new(password.into()),
        passphrase: Zeroizing::new(phrase.into()),
        jump_passphrase: Default::default(),
        use_saved: false,
        save_key: true,
    };
    let attempt = |key: Option<PathBuf>, secret: &str| {
        let (auth, prompts) = InteractiveAuth::new(|| {});
        let error = connect_location_at(request(key.clone(), secret), auth, &trust)
            .err()
            .expect("expected a local key import failure");
        assert!(prompts.try_recv().is_err());
        let message = crate::i18n::connection_result_error(&error);
        assert!(message.contains(&endpoint.label()), "{message}");
        if let Some(path) = key {
            assert!(message.contains(&*path.to_string_lossy()), "{message}");
        }
        for value in [message, error.to_string(), format!("{error:?}")] {
            assert!(!value.contains(password));
            assert!(!value.contains(phrase));
            assert!(!value.contains(std::str::from_utf8(prefix).unwrap()));
        }
        let TransferError::Failed(Failure::Connection(error)) = error else {
            panic!("expected a connection setup error");
        };
        let ConnectionError::Route {
            jump: false,
            address,
            cause,
        } = *error
        else {
            panic!("expected the target for the key import");
        };
        assert_eq!(address, endpoint.label());
        cause
    };
    assert!(
        matches!(attempt(None, password), TransferError::Failed(Failure::Connection(error))
        if matches!(*error, ConnectionError::KeyPasswordConflict))
    );
    assert!(
        matches!(attempt(None, ""), TransferError::Failed(Failure::Connection(error))
        if matches!(*error, ConnectionError::KeyFileRequired))
    );
    assert!(matches!(
        attempt(Some(absent.clone()), ""),
        TransferError::Failed(Failure::Access(error))
            if error.location.path() == absent
                && matches!(error.action, AccessAction::OpenFile)
                && matches!(&error.cause, AccessCause::Local(source)
                    if source.kind() == std::io::ErrorKind::NotFound)
    ));
    assert!(matches!(
        attempt(Some(root.to_owned()), ""),
        TransferError::Failed(Failure::Connection(error))
            if matches!(error.as_ref(), ConnectionError::ImportedKeyNotFile(path) if path.as_path() == root)
    ));
    assert!(matches!(
        attempt(Some(large.clone()), ""),
        TransferError::Failed(Failure::Connection(error))
            if matches!(error.as_ref(), ConnectionError::ImportedKeyTooLarge { path, limit: 65536 } if path == &large)
    ));
    let TransferError::Failed(Failure::Connection(encoding)) = attempt(Some(invalid.clone()), "")
    else {
        panic!("expected an encoding error with a safe source");
    };
    assert!(matches!(
        encoding.as_ref(),
        ConnectionError::ImportedKeyEncoding { path, source }
            if path == &invalid && source.valid_up_to() == prefix.len()
    ));
    assert!(encoding.source().unwrap().is::<std::str::Utf8Error>());

    let (auth, _) = InteractiveAuth::new(|| {});
    assert!(auth.control().cancel());
    assert!(matches!(
        connect_location_at(request(Some(invalid), ""), auth, &trust),
        Err(TransferError::Cancelled)
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn channel_failure_on_existing_session_preserves_the_endpoint_and_ssh_error() {
    use crate::file_error::{ConnectionError, Failure};
    use crate::vfs::sftp::cached_channel;
    use std::error::Error;

    let endpoint = Endpoint {
        host: "example.invalid".into(),
        port: 2222,
        user: "fixture".into(),
        key: None,
        jump: None,
    };
    let session = Arc::new(ssh2::Session::new().unwrap());
    let error = cached_channel(&endpoint, &session)
        .err()
        .expect("an unauthenticated session cannot open an SFTP channel");
    let TransferError::Failed(Failure::Connection(reason)) = &error else {
        panic!("missing typed connection error: {error:?}");
    };
    assert!(matches!(reason.as_ref(),
        ConnectionError::CachedChannel { endpoint: actual, source }
            if actual == &endpoint.label()
                && source.code() == ssh2::ErrorCode::Session(libssh2_sys::LIBSSH2_ERROR_INVAL)
    ));
    assert!(reason.source().unwrap().is::<ssh2::Error>());
    let message = crate::i18n::file_error(&error);
    assert!(message.contains(&endpoint.label()), "{message}");
    assert!(!message.contains("MFA"), "{message}");
}

#[cfg(target_os = "macos")]
fn check_sftp_connection_errors(root: &Path) {
    use crate::file_error::{ConnectionError, Failure, SshStage};
    use crate::vfs::{
        sftp::{connect_location_at, TrustFiles},
        ConnectRequest, InteractiveAuth,
    };
    use std::{error::Error, io::Write, net::TcpListener};

    let attempt = |host: &str, port| {
        let (auth, prompts) = InteractiveAuth::new(|| {});
        let request = ConnectRequest {
            endpoint: Endpoint {
                host: host.into(),
                port,
                user: "fixture".into(),
                key: None,
                jump: None,
            },
            path: ".".into(),
            password: Default::default(),
            passphrase: Default::default(),
            jump_passphrase: Default::default(),
            use_saved: false,
            save_key: false,
        };
        let trust = TrustFiles::new(root.join("known_hosts"), None);
        let error = connect_location_at(request, auth, &trust)
            .err()
            .expect("expected connection failure");
        assert!(prompts.try_recv().is_err());
        let TransferError::Failed(Failure::Connection(cause)) = error else {
            panic!("lost connection cause: {error:?}");
        };
        cause
    };

    let invalid = "not:an:ipv6:address";
    let resolve = attempt(invalid, 22);
    assert!(matches!(resolve.as_ref(), ConnectionError::Resolve { host, .. } if host == invalid));
    assert!(resolve.source().unwrap().is::<std::io::Error>());

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .write_all(b"SSH-2.0-fixture-without-key-exchange\r\n")
            .unwrap();
    });
    let handshake = attempt("127.0.0.1", port);
    server.join().unwrap();
    assert!(matches!(
        handshake.as_ref(),
        ConnectionError::Ssh {
            stage: SshStage::Handshake,
            ..
        }
    ));
    assert!(handshake.source().unwrap().is::<ssh2::Error>());
}

#[cfg(target_os = "macos")]
fn check_sftp_initial_folder(endpoint: &Endpoint, root: &Path) {
    use crate::file_error::{ConnectionError, Failure};
    use crate::vfs::{
        sftp::{connect_location_at, TrustFiles},
        ConnectRequest, InteractiveAuth,
    };
    use zeroize::Zeroizing;

    let fixture = tempfile::tempdir_in(root).unwrap();
    let folder = fixture.path().join("folder");
    let file = fixture.path().join("file");
    let alias = fixture.path().join("start-alias");
    fs::create_dir(&folder).unwrap();
    fs::write(&file, b"existing server file").unwrap();
    std::os::unix::fs::symlink(&file, &alias).unwrap();
    let connect = |path: &Path, save_key| {
        let (auth, prompts) = InteractiveAuth::new(|| {});
        let request = ConnectRequest {
            endpoint: endpoint.clone(),
            path: path.to_string_lossy().into_owned(),
            password: Default::default(),
            passphrase: Zeroizing::new("test-passphrase".into()),
            jump_passphrase: Default::default(),
            use_saved: false,
            save_key,
        };
        let result = connect_location_at(
            request,
            auth,
            &TrustFiles::new(root.join("known_hosts"), None),
        );
        assert!(prompts.try_recv().is_err());
        result
    };
    let prior = sessions().lock().unwrap()[endpoint].clone();
    // A valid key can authenticate in memory. Failure to validate the starting
    // folder must precede any attempt to save it or activate a new session.
    let error = connect(&alias, true)
        .err()
        .expect("expected a non-directory start");
    let TransferError::Failed(Failure::Connection(error)) = error else {
        panic!("expected the typed starting directory problem");
    };
    assert!(matches!(
        *error,
        ConnectionError::InitialPathNotDirectory(Location::Sftp { endpoint: actual, path })
            if &actual == endpoint && path == file.canonicalize().unwrap()
    ));
    assert!(std::sync::Arc::ptr_eq(
        &prior,
        &sessions().lock().unwrap()[endpoint]
    ));
    assert_eq!(fs::read(&file).unwrap(), b"existing server file");
    let connected = connect(&folder, false).unwrap();
    assert_eq!(connected.location.path(), folder.canonicalize().unwrap());
    assert!(!connected.saved_profile);
}

#[cfg(target_os = "macos")]
fn check_sftp_access_errors(remote_root: &Location) {
    use crate::file_error::{AccessAction, AccessCause, Failure};
    use std::os::unix::fs::{symlink, PermissionsExt};

    let missing = remote_root.join("missing-metadata-entry");
    assert!(entry_metadata(&missing).unwrap().is_none());
    let sftp = remote(remote_root).unwrap();
    let denied = remote_root.join("denied-metadata-folder");
    fs::create_dir(denied.path()).unwrap();
    let child = denied.join("file");
    fs::write(child.path(), b"preserved despite metadata denial").unwrap();
    let mode = fs::metadata(denied.path()).unwrap().permissions();
    fs::set_permissions(denied.path(), fs::Permissions::from_mode(0o600)).unwrap();
    let inspection = entry_metadata(&child);
    fs::set_permissions(denied.path(), mode).unwrap();
    let inspection = inspection
        .err()
        .expect("metadata must fail with permission denied");

    let source = remote_root.join("canonicalization-source");
    fs::write(source.path(), b"preserve during path check").unwrap();
    let link = remote_root.join("unresolvable-folder");
    symlink("unresolvable-folder", link.path()).unwrap();
    let blocked = link.join("subdirectory");
    let expected_resolution = sftp.realpath(blocked.path()).unwrap_err().code();
    let transfer = transfer_with_conflicts(
        std::slice::from_ref(&source),
        &blocked,
        TransferKind::Copy,
        &mut Progress::default(),
        |_| panic!("an invalid destination cannot reach a conflict"),
        |_, _| panic!("an invalid destination cannot publish"),
    )
    .unwrap_err();
    assert!(transfer.summary.preparing);
    assert_eq!(transfer.summary.completed, 0);
    assert_eq!(transfer.summary.remaining, std::slice::from_ref(&source));
    for (failure, location, action, code) in [
        (
            inspection,
            &child,
            AccessAction::ReadMetadata,
            ssh2::ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_PERMISSION_DENIED),
        ),
        (
            transfer.error,
            &blocked,
            AccessAction::ResolvePath,
            expected_resolution,
        ),
    ] {
        let TransferError::Failed(Failure::Access(details)) = &failure else {
            panic!("original SFTP failure was lost: {failure:?}");
        };
        assert_eq!(&details.location, location);
        assert_eq!(details.action, action);
        assert!(matches!(&details.cause, AccessCause::Sftp(error) if error.code() == code));
        assert!(crate::i18n::file_error(&failure).contains(&location.display()));
    }
    assert_eq!(
        fs::read(child.path()).unwrap(),
        b"preserved despite metadata denial"
    );
    assert_eq!(
        fs::read(source.path()).unwrap(),
        b"preserve during path check"
    );
    fs::remove_dir_all(denied.path()).unwrap();
    fs::remove_file(link.path()).unwrap();
    fs::remove_file(source.path()).unwrap();
}

#[cfg(target_os = "macos")]
fn check_sftp_preview_errors(remote_root: &Location) {
    use crate::file_error::{AccessAction, AccessCause, Failure};
    use std::os::unix::fs::PermissionsExt;

    let missing = remote_root.join("missing preview.txt");
    let denied = remote_root.join("unreadable preview.txt");
    let bytes = b"fixture kept after preview failures";
    fs::write(denied.path(), bytes).unwrap();
    let mode = fs::metadata(denied.path()).unwrap().permissions();
    fs::set_permissions(denied.path(), fs::Permissions::from_mode(0o000)).unwrap();
    let cancel = Cancellation::default();
    let absent = preview_data(&missing, 32, &cancel);
    let blocked = crate::vfs::open_preview_stream(&denied);
    fs::set_permissions(denied.path(), mode).unwrap();

    for (error, path, action, code) in [
        (
            absent.err().unwrap(),
            &missing,
            AccessAction::ReadMetadata,
            libssh2_sys::LIBSSH2_FX_NO_SUCH_FILE,
        ),
        (
            blocked.err().unwrap(),
            &denied,
            AccessAction::OpenFile,
            libssh2_sys::LIBSSH2_FX_PERMISSION_DENIED,
        ),
    ] {
        let TransferError::Failed(Failure::Access(details)) = &error else {
            panic!("lost preview access error: {error:?}");
        };
        assert_eq!(&details.location, path);
        assert_eq!(details.action, action);
        assert!(
            matches!(&details.cause, AccessCause::Sftp(source) if source.code() == ssh2::ErrorCode::SFTP(code))
        );
        assert!(crate::i18n::file_error(&error).contains(&path.display()));
    }

    let mut read_only_destination = fs::File::open(denied.path()).unwrap();
    let failed_cache =
        cache_media_preview(&denied, &mut read_only_destination, 64, &cancel).unwrap_err();
    assert!(
        matches!(failed_cache, TransferError::Failed(Failure::Access(details))
        if details.location == denied && details.action == AccessAction::WritePreviewCache
        && matches!(&details.cause, AccessCause::Local(source) if source.raw_os_error() == Some(libc::EBADF)))
    );

    let mut checks = 0;
    let cancel = Cancellation::at_checkpoint(move || {
        checks += 1;
        checks == 4
    });
    assert!(matches!(
        preview_data(&denied, 4, &cancel),
        Err(TransferError::Cancelled)
    ));
    assert_eq!(fs::read(denied.path()).unwrap(), bytes);
    fs::remove_file(denied.path()).unwrap();
}

fn check_sftp_deletion_results(remote_root: &Location) {
    use crate::file_error::{Failure, MutationError, SftpRemovalAction};
    use crate::result_history::Outcome;

    let first = remote_root.join("deleted-first");
    let missing = remote_root.join("missing-for-deletion");
    let later = remote_root.join("untouched-by-deletion");
    fs::write(first.path(), b"will be deleted").unwrap();
    fs::write(later.path(), b"must remain").unwrap();
    let mut progress = Progress::default();
    let failure = trash_entries_with_progress(
        &[first.clone(), missing.clone(), later.clone()],
        &mut progress,
    )
    .unwrap_err();
    assert!(matches!(
        failure.error,
        TransferError::Failed(Failure::Mutation(reason))
            if matches!(reason.as_ref(), MutationError::RemoveSftp {
                location, action: SftpRemovalAction::Inspect, source,
            } if location == &missing
                && source.code() == ssh2::ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_NO_SUCH_FILE))
    ));
    assert_eq!(failure.completed, 1);
    assert!(!failure.partial_remote);
    assert_eq!(
        progress
            .results
            .items
            .iter()
            .map(|row| row.outcome)
            .collect::<Vec<_>>(),
        [Outcome::Completed, Outcome::Failed, Outcome::NotStarted]
    );
    assert!(!first.path().exists());
    assert_eq!(fs::read(later.path()).unwrap(), b"must remain");
    fs::remove_file(later.path()).unwrap();

    let folder = remote_root.join("cancelled-deletion");
    let child = folder.join("confirmed-child");
    fs::create_dir(folder.path()).unwrap();
    fs::write(child.path(), b"will be deleted").unwrap();
    let child_path = child.path().to_owned();
    let cancel = crate::operations::Cancellation::at_checkpoint(move || !child_path.exists());
    let mut progress = Progress::with_cancellation(cancel);
    let failure =
        trash_entries_with_progress(std::slice::from_ref(&folder), &mut progress).unwrap_err();
    assert!(matches!(failure.error, TransferError::Cancelled));
    assert_eq!(failure.completed, 0);
    assert!(failure.partial_remote);
    assert_eq!(progress.results.items[0].outcome, Outcome::Cancelled);
    assert!(!progress.results.items[0].descendants_enumerated);
    assert_eq!(progress.results.items[1].outcome, Outcome::Completed);
    assert_eq!(progress.results.items[1].source, child.display());
    assert!(folder.path().is_dir());
    assert!(!child.path().exists());
    fs::remove_dir(folder.path()).unwrap();
}

#[cfg(target_os = "macos")]
fn check_promised_sftp_download(local_root: &Location, remote_root: &Location) {
    use crate::{
        file_error::{Failure, FilePromiseError, MutationError},
        result_history::Outcome,
        vfs::{copy_sftp_for_drop, resume_copy, DropDelivery, PromiseKind},
    };

    let output = local_root.join("receiving app");
    let recovery = local_root.path().join("promised-downloads");
    fs::create_dir(output.path()).unwrap();
    let source = remote_root.join("drag source.txt");
    let target = output.join("drag source 2.txt");
    let bytes = b"file delivered at the receiver's requested name";
    fs::write(source.path(), bytes).unwrap();
    fs::write(output.join("drag source.txt").path(), b"existing file").unwrap();
    let mut progress = Progress::default();
    progress.resume_directory = Some(recovery.clone());
    assert_eq!(
        copy_sftp_for_drop(&source, target.path(), PromiseKind::File, &mut progress).unwrap(),
        DropDelivery::Completed
    );
    assert_eq!(fs::read(target.path()).unwrap(), bytes);
    assert_eq!(fs::read(source.path()).unwrap(), bytes);
    assert_eq!(
        fs::read(output.join("drag source.txt").path()).unwrap(),
        b"existing file"
    );
    let error = copy_sftp_for_drop(
        &source,
        target.path(),
        PromiseKind::File,
        &mut Progress::default(),
    )
    .unwrap_err();
    assert!(
        matches!(error, TransferError::Failed(Failure::Mutation(reason))
        if matches!(reason.as_ref(), MutationError::DestinationExists(path) if path == &target))
    );
    assert_eq!(fs::read(target.path()).unwrap(), bytes);

    let source_folder = remote_root.join("drag folder");
    let target_folder = output.join("drag folder");
    fs::create_dir_all(source_folder.join("nested/empty").path()).unwrap();
    fs::write(source_folder.join("nested/child.txt").path(), bytes).unwrap();
    let missing = remote_root.join("missing drag source.txt");
    let mut progress = Progress::default();
    let error = copy_sftp_for_drop(
        &missing,
        target_folder.path(),
        PromiseKind::File,
        &mut progress,
    )
    .unwrap_err();
    assert!(
        matches!(error, TransferError::Failed(Failure::FilePromise(reason))
        if matches!(reason.as_ref(), FilePromiseError::SourceMissing(path) if path == &missing))
    );
    assert_eq!(
        progress.results.root_outcomes(),
        [(missing, Outcome::Failed)]
    );
    let mut progress = Progress::default();
    let error = copy_sftp_for_drop(
        &source_folder,
        target_folder.path(),
        PromiseKind::File,
        &mut progress,
    )
    .unwrap_err();
    assert!(
        matches!(error, TransferError::Failed(Failure::FilePromise(reason))
        if matches!(reason.as_ref(), FilePromiseError::SourceTypeChanged(path) if path == &source_folder))
    );
    assert_eq!(
        progress.results.root_outcomes(),
        [(source_folder.clone(), Outcome::Failed)]
    );
    assert!(!target_folder.path().exists());

    let mut progress = Progress::default();
    progress.resume_directory = Some(recovery.clone());
    copy_sftp_for_drop(
        &source_folder,
        target_folder.path(),
        PromiseKind::Directory,
        &mut progress,
    )
    .unwrap();
    assert!(target_folder.join("nested/empty").path().is_dir());
    assert_eq!(
        fs::read(target_folder.join("nested/child.txt").path()).unwrap(),
        bytes
    );
    assert_eq!(
        fs::read(source_folder.join("nested/child.txt").path()).unwrap(),
        bytes
    );

    let source = remote_root.join("stopped download.bin");
    let target = output.join("stopped download.bin");
    let data = vec![53; 1024 * 1024];
    fs::write(source.path(), &data).unwrap();
    let destination = output.path().to_owned();
    let mut progress = Progress::default();
    progress.resume_directory = Some(recovery.clone());
    progress.cancellation().set_checkpoint(move || {
        fs::read_dir(&destination)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pawcommander-transfer-")
                    && fs::metadata(entry.path().join("payload")).is_ok_and(|meta| meta.len() != 0)
            })
    });
    assert!(matches!(
        copy_sftp_for_drop(&source, target.path(), PromiseKind::File, &mut progress),
        Err(TransferError::Cancelled)
    ));
    assert_eq!(
        progress.results.root_outcomes(),
        [(source.clone(), Outcome::Cancelled)]
    );
    assert_eq!(fs::read(source.path()).unwrap(), data);
    assert!(!target.path().exists());
    let (offers, errors) = resume_copy::scan(&recovery);
    assert!(offers.is_empty() && errors.is_empty());
    assert_eq!(fs::read_dir(output.path()).unwrap().count(), 3);
    println!("SFTP file and folder delivery honored exact names and preserved originals. Occupied destinations, vanished or changed items, and Stop returned distinct typed errors");
}

#[cfg(target_os = "macos")]
fn check_changed_promised_source_after_preflight(local_root: &Location, remote_root: &Location) {
    use crate::{
        file_error::{Failure, FilePromiseError},
        result_history::Outcome,
        vfs::{copy_sftp_for_drop, resume_copy, PromiseKind},
    };

    let receiver = local_root.join("late promise receiver");
    fs::create_dir(receiver.path()).unwrap();
    let sentinel = receiver.join("previous file");
    fs::write(sentinel.path(), b"existing destination bytes").unwrap();
    let recovery = local_root.path().join("late-promise-receipts");
    let bytes = b"original source bytes";
    for (case, promise, resumable) in [
        ("file-plain", PromiseKind::File, false),
        ("folder-plain", PromiseKind::Directory, false),
        ("file-resumable", PromiseKind::File, true),
        ("folder-resumable", PromiseKind::Directory, true),
    ] {
        let source = remote_root.join(case);
        let saved = remote_root.join(format!("{case}-original"));
        let target = receiver.join(case);
        if promise == PromiseKind::File {
            fs::write(source.path(), bytes).unwrap();
        } else {
            fs::create_dir(source.path()).unwrap();
            fs::write(source.join("child").path(), bytes).unwrap();
        }
        let mut progress = Progress::default();
        progress.resume_directory = resumable.then(|| recovery.clone());
        let mut checks = 0;
        let source_path = source.path().to_owned();
        let saved_path = saved.path().to_owned();
        progress.cancellation().set_checkpoint(move || {
            checks += 1;
            if checks == 2 {
                // Swap the source between the preflight check and copy dispatch.
                fs::rename(&source_path, &saved_path).unwrap();
                if promise == PromiseKind::File {
                    fs::create_dir(&source_path).unwrap();
                } else {
                    fs::write(&source_path, b"replacement regular file").unwrap();
                }
            }
            false
        });
        let error = copy_sftp_for_drop(&source, target.path(), promise, &mut progress).unwrap_err();
        assert!(
            matches!(error, TransferError::Failed(Failure::FilePromise(reason))
            if matches!(reason.as_ref(), FilePromiseError::SourceTypeChanged(path) if path == &source))
        );
        assert_eq!(
            progress.results.root_outcomes(),
            [(source.clone(), Outcome::Failed)]
        );
        assert!(!target.path().exists());
        if promise == PromiseKind::File {
            assert!(source.path().is_dir());
            assert_eq!(fs::read(saved.path()).unwrap(), bytes);
        } else {
            assert_eq!(
                fs::read(source.path()).unwrap(),
                b"replacement regular file"
            );
            assert_eq!(fs::read(saved.join("child").path()).unwrap(), bytes);
        }
    }
    assert_eq!(
        fs::read(sentinel.path()).unwrap(),
        b"existing destination bytes"
    );
    assert_eq!(fs::read_dir(receiver.path()).unwrap().count(), 1);
    let (offers, errors) = resume_copy::scan(&recovery);
    assert!(offers.is_empty() && errors.is_empty());
    println!("SFTP promises rejected both late kind changes in regular and resumable copies without writing to the receiver");
}

#[cfg(target_os = "macos")]
fn check_sftp_editor(local_root: &Location, remote_root: &Location, setstat_denied: bool) {
    use crate::file_error::{AccessAction, AccessCause, Failure, RemoteEditError};
    use crate::result_history::Outcome;
    use crate::vfs::RemoteEditSaveOutcome;
    use std::os::unix::fs::PermissionsExt;

    let directory = remote_root.with_path(remote_root.path().with_file_name("editor"));
    fs::create_dir(directory.path()).unwrap();
    let source = directory.join("a 'quoted' file.txt");
    fs::write(source.path(), b"original").unwrap();
    fs::set_permissions(source.path(), fs::Permissions::from_mode(0o640)).unwrap();
    let copies = local_root.path().with_file_name("editor-copies");
    let mut edit =
        crate::vfs::RemoteEdit::download(source.clone(), &copies, &mut Progress::default())
            .unwrap();
    let server_path = source.with_path(fs::canonicalize(source.path()).unwrap());
    assert_eq!(edit.target(), &server_path);
    assert_eq!(fs::read(edit.copy()).unwrap(), b"original");
    assert_eq!(
        fs::metadata(edit.copy().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    fs::write(edit.copy(), b"edited!!").unwrap();
    let mut cancelled = Progress::default();
    cancelled.cancellation().request();
    assert!(matches!(
        edit.save(&mut cancelled),
        Err(TransferError::Cancelled)
    ));
    assert!(cancelled.results.cancelled);
    assert_eq!(cancelled.results.items[0].outcome, Outcome::Cancelled);
    assert_eq!(fs::read(source.path()).unwrap(), b"original");

    let mut progress = Progress::default();
    let result = edit.save(&mut progress);
    if setstat_denied {
        assert!(
            matches!(result, Err(TransferError::Failed(Failure::Access(error)))
            if error.action == AccessAction::WriteMetadata
                && error.location.path().ends_with("payload")
                && matches!(&error.cause, AccessCause::Sftp(source) if matches!(source.code(), ssh2::ErrorCode::SFTP(_))))
        );
        assert_eq!(progress.results.items[0].outcome, Outcome::Failed);
        assert_eq!(fs::read(source.path()).unwrap(), b"original");
    } else {
        assert!(matches!(
            result.unwrap(),
            RemoteEditSaveOutcome::Saved {
                cleanup_warning: None
            }
        ));
        assert_eq!(progress.results.items[0].outcome, Outcome::Completed);
        assert_eq!(fs::read(source.path()).unwrap(), b"edited!!");
        assert_eq!(
            fs::metadata(source.path()).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(matches!(
            edit.save(&mut Progress::default()).unwrap(),
            RemoteEditSaveOutcome::AlreadyCurrent
        ));
        let modified = fs::metadata(source.path()).unwrap().modified().unwrap();
        fs::write(source.path(), b"external").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(source.path())
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        fs::write(edit.copy(), b"new edit").unwrap();
        let conflict = edit.save(&mut Progress::default()).unwrap_err();
        assert!(
            matches!(
                &conflict,
                TransferError::Failed(Failure::RemoteEdit(error))
                    if matches!(error.as_ref(), RemoteEditError::RevisionChanged(path) if path == &server_path)
            ),
            "{conflict:?}"
        );
        assert_eq!(fs::read(source.path()).unwrap(), b"external");
    }
    assert_eq!(
        fs::read(edit.copy()).unwrap(),
        if setstat_denied {
            b"edited!!"
        } else {
            b"new edit"
        }
    );
    let moved_copy = edit.copy().with_extension("retained");
    let server_before = fs::read(source.path()).unwrap();
    fs::rename(edit.copy(), &moved_copy).unwrap();
    assert!(matches!(
        edit.save(&mut Progress::default()).unwrap_err(),
        TransferError::Failed(Failure::RemoteEdit(error))
            if matches!(error.as_ref(), RemoteEditError::WorkingCopyMissing(path) if path == edit.copy())
    ));
    assert_eq!(fs::read(source.path()).unwrap(), server_before);
    fs::rename(moved_copy, edit.copy()).unwrap();
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

fn check_sftp_retry(local_root: &Location, remote_root: &Location) {
    for (name, source_root, target_root, kind) in [
        ("upload", local_root, remote_root, TransferKind::Copy),
        ("download", remote_root, local_root, TransferKind::Copy),
        ("remote-copy", remote_root, remote_root, TransferKind::Copy),
        ("remote-move", remote_root, remote_root, TransferKind::Move),
        ("upload-move", local_root, remote_root, TransferKind::Move),
        ("download-move", remote_root, local_root, TransferKind::Move),
    ] {
        let source = source_root.join(format!("retry-{name}-source"));
        let target = target_root.join(format!("retry-{name}-target"));
        fs::create_dir_all(source.join("tree/empty").path()).unwrap();
        fs::create_dir(target.path()).unwrap();
        fs::write(source.join("a").path(), "completed").unwrap();
        fs::write(source.join("b").path(), "retry payload").unwrap();
        fs::write(source.join("tree/nested").path(), "nested payload").unwrap();
        fs::write(target.join("b").path(), "old destination").unwrap();
        let mut progress = Progress::default();
        progress.retry = Some(crate::vfs::RetryTransfer::new(kind, target.clone()));
        let summary = transfer_with_conflicts(
            &[source.join("a"), source.join("b"), source.join("tree")],
            &target,
            kind,
            &mut progress,
            |_| {
                Ok(Answer {
                    decision: Decision::Skip,
                    apply_to_group: false,
                })
            },
            |_, _| {},
        )
        .unwrap();
        assert_eq!((summary.completed, summary.skipped), (2, 1), "{name}");
        let retry = progress
            .retry
            .take()
            .unwrap()
            .unfinished(&progress.results)
            .unwrap();
        assert_eq!(retry.sources(), vec![source.join("b")]);
        fs::write(source.join("a").path(), "independent newer source").unwrap();
        let mut progress = Progress::default();
        progress.retry = Some(retry);
        let summary = transfer_with_conflicts(
            &[source.join("b")],
            &target,
            kind,
            &mut progress,
            |_| {
                Ok(Answer {
                    decision: Decision::KeepBoth,
                    apply_to_group: false,
                })
            },
            |_, _| {},
        )
        .unwrap();
        assert_eq!(summary.completed, 1, "{name}");
        assert!(progress
            .retry
            .take()
            .unwrap()
            .unfinished(&progress.results)
            .is_none());
        assert_eq!(fs::read(target.join("a").path()).unwrap(), b"completed");
        assert_eq!(
            fs::read(source.join("a").path()).unwrap(),
            b"independent newer source"
        );
        assert!(!target.join("a (2)").path().exists());
        assert_eq!(
            fs::read(target.join("b").path()).unwrap(),
            b"old destination"
        );
        assert_eq!(
            fs::read(target.join("b (2)").path()).unwrap(),
            b"retry payload"
        );
        assert_eq!(source.join("b").path().exists(), kind == TransferKind::Copy);
        assert_eq!(
            fs::read(target.join("tree/nested").path()).unwrap(),
            b"nested payload"
        );
        assert!(target.join("tree/empty").path().is_dir());
        assert!(!fs::read_dir(target.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".pawcommander")));
        fs::remove_dir_all(source.path()).unwrap();
        fs::remove_dir_all(target.path()).unwrap();
    }
}

#[cfg(unix)]
fn check_sftp_unsupported_names(local_root: &Location, remote_root: &Location) {
    for direction in ["upload", "download", "remote-copy"] {
        let (source_root, target_root) = match direction {
            "upload" => (local_root, remote_root),
            "download" => (remote_root, local_root),
            _ => (remote_root, remote_root),
        };
        for (index, name) in unsupported_sftp_names().into_iter().enumerate() {
            for late in [false, true] {
                let source =
                    source_root.join(format!("invalid-name-{direction}-{index}-{late}-source"));
                let target =
                    target_root.join(format!("invalid-name-{direction}-{index}-{late}-target"));
                fs::create_dir_all(source.join("nested").path()).unwrap();
                fs::write(source.join("nested/valid").path(), b"valid source").unwrap();
                let unsupported = source.join("nested").join(&name);
                if !late {
                    fs::write(unsupported.path(), b"unsupported source").unwrap();
                }
                fs::write(target.path(), b"previous destination").unwrap();
                let expected = entry_metadata(&target).unwrap().unwrap();
                let mut copied = false;
                let error = copy_transfer_checked(
                    &TransferItem::new(source.clone(), source.clone(), target.clone()),
                    Some(&expected),
                    &mut Progress::default(),
                    &mut |path, phase| {
                        assert!(late, "preflight must reject names before copying");
                        if path == &source && phase == CopyPhase::EntryCopied {
                            copied = true;
                            fs::write(unsupported.path(), b"unsupported source").unwrap();
                        }
                        Ok(())
                    },
                )
                .unwrap_err();
                assert_eq!(copied, late);
                // A directory metadata change may be observed before its
                // new unsupported name; either must abort and clean up.
                assert!(
                    error.contains("SFTP") || error.contains("changed during copying"),
                    "{error}"
                );
                assert!(
                    !error.contains("Temporary folder remains at"),
                    "{direction}/{index}/{late}: {error}"
                );
                assert_eq!(fs::read(target.path()).unwrap(), b"previous destination");
                assert_eq!(fs::read(unsupported.path()).unwrap(), b"unsupported source");
                assert_eq!(
                    fs::read(source.join("nested/valid").path()).unwrap(),
                    b"valid source"
                );
                assert!(fs::read_dir(target_root.path()).unwrap().all(|entry| {
                    let name = entry.unwrap().file_name();
                    let name = name.to_string_lossy();
                    !name.starts_with(".pawcommander-sftp-")
                        && !name.starts_with(".pawcommander-transfer-")
                }));
                // The destination parent remains readable by the UI.
                read_directory(target_root, &|| false).unwrap();
            }
        }
    }
}

fn check_sftp_source_change_cleanup_at_depth_limit(local_root: &Location, remote_root: &Location) {
    for direction in ["upload", "remote-copy"] {
        let source_root = if direction == "upload" {
            local_root
        } else {
            remote_root
        };
        for depth in [128, 129] {
            let source = source_root.join(format!("deep-{direction}-{depth}-source"));
            let target = remote_root.join(format!("deep-{direction}-{depth}-target"));
            let sibling = remote_root.join(format!("deep-{direction}-{depth}-existing"));
            let mut leaf = source.clone();
            for _ in 1..depth {
                leaf = leaf.join("d");
            }
            fs::create_dir_all(leaf.path()).unwrap();
            leaf = leaf.join("file");
            fs::write(leaf.path(), b"original").unwrap();
            fs::write(sibling.path(), b"existing destination sibling").unwrap();
            let mut copied = false;
            let error = copy_transfer_checked(
                &TransferItem::new(source.clone(), source.clone(), target.clone()),
                None,
                &mut Progress::default(),
                &mut |path, phase| {
                    if path == &source && phase == CopyPhase::EntryCopied {
                        copied = true;
                        fs::write(leaf.path(), b"changed source").unwrap();
                    }
                    Ok(())
                },
            )
            .unwrap_err();
            assert_eq!(copied, depth == 128, "{direction}/{depth}: {error}");
            assert!(
                if copied {
                    error.contains("changed during copying")
                } else {
                    error.contains(&crate::tr_format!(
                        "Folders are nested more than {limit} levels at {path}.",
                        limit = crate::vfs::MAX_TREE_DEPTH,
                        path = leaf
                    ))
                },
                "{direction}/{depth}: {error}"
            );
            assert!(
                !error.contains("Temporary folder remains at"),
                "{direction}/{depth}: {error}"
            );
            assert!(!target.path().exists());
            assert_eq!(
                fs::read(sibling.path()).unwrap(),
                b"existing destination sibling"
            );
            assert_eq!(
                fs::read(leaf.path()).unwrap(),
                if copied {
                    b"changed source".as_slice()
                } else {
                    b"original"
                }
            );
            assert!(fs::read_dir(remote_root.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pawcommander-sftp-")
            }));
            if direction == "remote-copy" {
                // The extra staging allowance must not expand the depth
                // supported by ordinary user-requested remote deletion.
                let removed = trash_entries(std::slice::from_ref(&source));
                assert_eq!(removed.is_ok(), depth == 128);
                assert_eq!(leaf.path().exists(), depth == 129);
            }
        }
    }
}

// Uses only disposable loopback fixtures. Mutations are synchronized with
// copy checkpoints rather than sleeps or probabilistic background writers.
fn check_sftp_source_changes(local_root: &Location, remote_root: &Location) {
    for direction in ["upload", "download", "remote-copy"] {
        let (source_root, target_root) = match direction {
            "upload" => (local_root, remote_root),
            "download" => (remote_root, local_root),
            _ => (remote_root, remote_root),
        };
        for mutation in [
            "append",
            "truncate",
            "same-size-mtime",
            "opened-handle",
            "early-file",
            "add-child",
            "remove-child",
            "rename-child",
        ] {
            let source = source_root.join(format!("{direction}-{mutation}-source"));
            let target = target_root.join(format!("{direction}-{mutation}-target"));
            let tree = matches!(
                mutation,
                "early-file" | "add-child" | "remove-child" | "rename-child"
            );
            if tree {
                fs::create_dir_all(source.join("a-nested").path()).unwrap();
                fs::write(source.join("a-nested/early").path(), b"early").unwrap();
                fs::write(source.join("z-late").path(), b"late").unwrap();
            } else {
                fs::write(source.path(), b"original").unwrap();
            }
            fs::write(target.path(), b"previous destination").unwrap();
            let expected = entry_metadata(&target).unwrap().unwrap();
            let mut mutated = false;
            let error = copy_transfer_checked(
                &TransferItem::new(source.clone(), source.clone(), target.clone()),
                Some(&expected),
                &mut Progress::default(),
                &mut |path, phase| {
                    let checkpoint = if mutation == "opened-handle" {
                        CopyPhase::Opened
                    } else if tree {
                        CopyPhase::EntryCopied
                    } else {
                        CopyPhase::Read
                    };
                    let path_matches = if tree {
                        path == &source.join("z-late")
                    } else {
                        path == &source
                    };
                    if mutated || phase != checkpoint || !path_matches {
                        return Ok(());
                    }
                    mutated = true;
                    match mutation {
                        "append" => fs::write(source.path(), b"original grew").unwrap(),
                        "truncate" => fs::write(source.path(), b"short").unwrap(),
                        "same-size-mtime" => {
                            fs::write(source.path(), b"modified").unwrap();
                            fs::File::options()
                                .write(true)
                                .open(source.path())
                                .unwrap()
                                .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
                                .unwrap();
                        }
                        "opened-handle" => {
                            let metadata = fs::metadata(source.path()).unwrap();
                            let retained = source_root.join(format!("{direction}-retained-inode"));
                            fs::rename(source.path(), retained.path()).unwrap();
                            fs::write(source.path(), b"original").unwrap();
                            fs::File::options()
                                .write(true)
                                .open(source.path())
                                .unwrap()
                                .set_times(
                                    fs::FileTimes::new().set_modified(metadata.modified().unwrap()),
                                )
                                .unwrap();
                            // lstat now sees an indistinguishable SFTP file,
                            // but fstat must see the changed open inode.
                            fs::write(retained.path(), b"changed opened inode").unwrap();
                        }
                        "early-file" => {
                            fs::write(source.join("a-nested/early").path(), b"early changed")
                                .unwrap()
                        }
                        "add-child" => {
                            fs::write(source.join("a-nested/new").path(), b"new").unwrap()
                        }
                        "remove-child" => {
                            fs::remove_file(source.join("a-nested/early").path()).unwrap()
                        }
                        "rename-child" => fs::rename(
                            source.join("a-nested/early").path(),
                            source.join("a-nested/renamed").path(),
                        )
                        .unwrap(),
                        _ => unreachable!(),
                    }
                    Ok(())
                },
            )
            .unwrap_err();
            assert!(mutated, "{direction}/{mutation}");
            assert!(
                error.contains("changed during copying"),
                "{direction}/{mutation}: {error}"
            );
            assert_eq!(fs::read(target.path()).unwrap(), b"previous destination");
            assert!(source.path().exists());
            assert!(fs::read_dir(target_root.path()).unwrap().all(|entry| {
                let name = entry.unwrap().file_name();
                let name = name.to_string_lossy();
                !name.starts_with(".pawcommander-sftp-")
                    && !name.starts_with(".pawcommander-transfer-")
            }));
        }
    }
}

fn check_sftp_cancellation(local_root: &Location, remote_root: &Location) {
    let bytes = vec![0x6a; 3 * 64 * 1024];
    for (direction, from_root, to_root) in [
        ("upload", local_root, remote_root),
        ("download", remote_root, local_root),
        ("remote-copy", remote_root, remote_root),
    ] {
        for replace in [false, true] {
            let name = format!("cancel-{direction}-{replace}");
            let from = create_directory(from_root, &format!("{name}-from")).unwrap();
            let to = create_directory(to_root, &format!("{name}-to")).unwrap();
            let source = from.join("payload");
            let target = to.join("payload");
            fs::write(source.path(), &bytes).unwrap();
            if replace {
                fs::write(target.path(), b"previous target").unwrap();
            }
            let inspect = to.path().to_path_buf();
            let cancel = Cancellation::at_checkpoint(move || {
                fs::read_dir(&inspect)
                    .unwrap()
                    .filter_map(Result::ok)
                    .any(|entry| {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        (name.starts_with(".pawcommander-sftp-")
                            || name.starts_with(".pawcommander-transfer-"))
                            && fs::metadata(entry.path().join("payload"))
                                .is_ok_and(|meta| meta.len() > 0)
                    })
            });
            let mut progress = Progress::with_cancellation(cancel);
            let report = transfer_with_conflicts(
                std::slice::from_ref(&source),
                &to,
                TransferKind::Copy,
                &mut progress,
                |_| {
                    Ok(Answer {
                        decision: Decision::Replace,
                        apply_to_group: false,
                    })
                },
                |_, _| panic!("copy"),
            )
            .unwrap();
            assert!(report.cancelled && !report.preparing, "{name}");
            assert_eq!((report.completed, report.skipped), (0, 0), "{name}");
            assert_eq!(report.remaining, vec![source.clone()], "{name}");
            assert_eq!(fs::read(source.path()).unwrap(), bytes, "{name}");
            if replace {
                assert_eq!(
                    fs::read(target.path()).unwrap(),
                    b"previous target",
                    "{name}"
                );
            } else {
                assert!(!target.path().exists(), "{name}");
            }
            assert_eq!(
                fs::read_dir(to.path()).unwrap().count(),
                usize::from(replace),
                "{name}: staging removed"
            );
            eprintln!("cancellation {name}: source/target/remainder/cleanup passed");
        }
    }
    for count in [1, 2] {
        let from = create_directory(remote_root, &format!("cancel-move-{count}-from")).unwrap();
        let to = create_directory(remote_root, &format!("cancel-move-{count}-to")).unwrap();
        let sources: Vec<_> = (0..count)
            .map(|index| from.join(format!("file-{index}")))
            .collect();
        for source in &sources {
            fs::write(source.path(), &bytes).unwrap();
        }
        fs::write(to.join("file-0").path(), b"previous target").unwrap();
        let mut progress = Progress::default();
        let cancel = progress.cancellation();
        let mut moved = Vec::new();
        let report = transfer_with_conflicts(
            &sources,
            &to,
            TransferKind::Move,
            &mut progress,
            |_| {
                Ok(Answer {
                    decision: Decision::Replace,
                    apply_to_group: false,
                })
            },
            |source, _| {
                moved.push(source.clone());
                cancel.request();
            },
        )
        .unwrap();
        assert_eq!(report.completed, 1);
        assert_eq!(report.cancelled, count != 1);
        assert_eq!(report.remaining, sources[1..]);
        assert_eq!(moved, vec![sources[0].clone()]);
        assert!(!sources[0].path().exists());
        assert_eq!(fs::read(to.join("file-0").path()).unwrap(), bytes);
        assert_eq!(
            fs::read_dir(to.path()).unwrap().count(),
            1,
            "remote move cleanup"
        );
        for source in &sources[1..] {
            assert_eq!(fs::read(source.path()).unwrap(), bytes);
        }
        eprintln!(
            "cancellation remote move {count}: commit/event/remainder/late-stop/cleanup passed"
        );
    }
}

fn check_sftp_copy_hash_verification(local_root: &Location, remote_root: &Location) {
    use std::sync::atomic::{AtomicBool, Ordering};
    let local = create_directory(local_root, "hash-local").unwrap();
    let remote_dir = create_directory(remote_root, "hash-remote").unwrap();
    let bytes = vec![0x5a; 256 * 1024];
    for directory in [&local, &remote_dir] {
        fs::write(directory.join("source").path(), &bytes).unwrap();
    }
    for (name, source, parent) in [
        ("upload", local.join("source"), &remote_dir),
        ("download", remote_dir.join("source"), &local),
        ("remote", remote_dir.join("source"), &remote_dir),
    ] {
        for stop in [false, true] {
            let target = parent.join(format!("{name}-{stop}"));
            fs::write(target.path(), b"old destination").unwrap();
            let expected = entry_metadata(&target).unwrap().unwrap();
            let armed = Arc::new(AtomicBool::new(false));
            let watch = armed.clone();
            let mut checks = 0;
            let cancel = Cancellation::at_checkpoint(move || {
                if watch.load(Ordering::Relaxed) {
                    checks += 1;
                }
                // Verifying is armed immediately before readback: check1
                // precedes the first read, check2 follows that actual read.
                stop && checks == 2
            });
            let mut progress = Progress::with_cancellation(cancel);
            progress.set_copy_verification(true);
            let result = copy_transfer_cancellable(
                &TransferItem::new(source.clone(), source.clone(), target.clone()),
                Some(&expected),
                &mut progress,
                &mut |_, phase| {
                    if phase == CopyPhase::Verifying {
                        armed.store(true, Ordering::Relaxed);
                        if !stop {
                            let stage = fs::read_dir(parent.path())
                                .unwrap()
                                .map(Result::unwrap)
                                .find(|entry| {
                                    entry
                                        .file_name()
                                        .to_string_lossy()
                                        .starts_with(".pawcommander-")
                                })
                                .unwrap();
                            fs::write(stage.path().join("payload"), vec![0x42; bytes.len()])
                                .unwrap();
                        }
                    }
                    Ok(())
                },
                || panic!("failed verification must not publish"),
            );
            if stop {
                assert!(
                    matches!(result, Err(TransferError::Cancelled)),
                    "{name}: {result:?}"
                );
            } else {
                assert!(
                    result.unwrap_err().to_string().contains("SHA-256"),
                    "{name}"
                );
            }
            assert_eq!(fs::read(source.path()).unwrap(), bytes, "{name}: source");
            assert_eq!(
                fs::read(target.path()).unwrap(),
                b"old destination",
                "{name}: target"
            );
            assert!(
                fs::read_dir(parent.path())
                    .unwrap()
                    .map(Result::unwrap)
                    .all(|entry| !entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".pawcommander-")),
                "{name}: staging"
            );
        }
    }
}

fn check_cross_connection_partial_move(local_root: &Location, remote_root: &Location) {
    use crate::result_history::Outcome;
    for (name, from, to) in [
        ("upload", local_root, remote_root),
        ("download", remote_root, local_root),
    ] {
        let source = from.join(format!("partial-move-{name}-source"));
        let target = to.join(format!("partial-move-{name}-target"));
        fs::create_dir_all(source.path()).unwrap();
        fs::create_dir_all(target.path()).unwrap();
        fs::write(source.join("first").path(), b"first payload").unwrap();
        fs::write(source.join("late").path(), vec![64; 128 * 1024]).unwrap();
        let published_late = target.join("late");
        let cancel = Cancellation::at_checkpoint(move || published_late.path().exists());
        let mut progress = Progress::with_cancellation(cancel);
        progress.retry = Some(crate::vfs::RetryTransfer::new(
            TransferKind::Move,
            target.clone(),
        ));
        let mut summary = TransferSummary::default();
        let mut consumed = Vec::new();
        let result = super::super::transfer::transfer_run(
            &[source.join("first"), source.join("late")],
            &target,
            TransferKind::Move,
            &mut progress,
            |_| panic!("unexpected conflict"),
            |path, _| consumed.push(path.clone()),
            &mut summary,
        );
        assert!(result.is_err(), "{name}");
        assert_eq!(
            summary.completed, 1,
            "published copy is not a completed move"
        );
        assert_eq!(summary.remaining, vec![source.join("late")]);
        assert_eq!(consumed, vec![source.join("first")]);
        assert!(!source.join("first").path().exists());
        assert_eq!(
            fs::read(target.join("first").path()).unwrap(),
            b"first payload"
        );
        assert_eq!(
            fs::read(source.join("late").path()).unwrap(),
            vec![64; 128 * 1024]
        );
        assert_eq!(
            fs::read(target.join("late").path()).unwrap(),
            vec![64; 128 * 1024]
        );
        assert_eq!(
            progress
                .results
                .items
                .iter()
                .map(|row| row.outcome)
                .collect::<Vec<_>>(),
            vec![Outcome::Completed, Outcome::CopiedSourceRemaining]
        );
        assert_eq!(progress.undo.len(), 0);
        assert!(progress.results.cancelled);
        let check = progress
            .retry
            .take()
            .unwrap()
            .unfinished(&progress.results)
            .unwrap();
        assert!(check.needs_inspection());
        assert_eq!(check.sources(), [source.join("late")]);
        let mut review = Progress::default();
        let (ready, pending) = check.inspect(&mut review).unwrap();
        assert!(ready.is_none() && pending.is_none());
        assert_eq!(
            review.results.items[0].outcome,
            Outcome::CopiedSourceRemaining
        );
        assert!(fs::read_dir(source.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pawcommander-")));
    }
}

fn check_sftp_archive_round_trip(remote_root: &Location) {
    use crate::archive::{Format, Index};
    use crate::result_history::Outcome;
    use crate::vfs::{create_archive, edit_archive, extract_archive, ArchiveEdit};

    let parent = remote_root.join("archive-creation");
    let source = parent.join("Folder");
    fs::create_dir_all(source.join("Empty").path()).unwrap();
    let file = source.join("file.txt");
    fs::write(file.path(), b"SFTP archive contents").unwrap();
    for (format, name, password) in [
        (Format::Zip, "created.zip", None),
        (Format::Tar, "created.tar", None),
        (
            Format::Zip,
            "protected.zip",
            Some("disposable SFTP archive secret"),
        ),
    ] {
        let mut progress = Progress::default();
        create_archive(
            std::slice::from_ref(&source),
            &parent,
            name,
            format,
            password,
            &mut progress,
        )
        .unwrap();
        assert_eq!(progress.results.items[0].outcome, Outcome::Completed);
        // On this loopback fixture, an independent local read checks the server's file.
        let target = parent.join(name);
        let local_target = Location::Local(target.path().into());
        let location = local_target.browse_archive().unwrap().join("Folder");
        let index = Index::load(local_target.path(), &|| false).unwrap();
        let files = index.list(&location, &|| false).unwrap();
        assert!(files
            .iter()
            .any(|entry| entry.name == "Empty" && entry.is_dir));
        assert!(files
            .iter()
            .any(|entry| entry.name == "file.txt" && entry.size == 21));
        let created = fs::read(target.path()).unwrap();
        assert!(create_archive(
            std::slice::from_ref(&source),
            &parent,
            name,
            format,
            password,
            &mut progress
        )
        .is_err());
        assert_eq!(fs::read(target.path()).unwrap(), created);
        edit_archive(
            local_target.path(),
            ArchiveEdit::Add {
                folder: PathBuf::new(),
                sources: vec![file.clone()],
            },
            password,
            &mut progress,
        )
        .unwrap();
        let archive_root = local_target.browse_archive().unwrap();
        let extracted = parent.join(format!("extracted-{name}"));
        fs::create_dir(extracted.path()).unwrap();
        extract_archive(
            &[location, archive_root.join("file.txt")],
            &extracted,
            password,
            &mut progress,
            |_| Err("unexpected extraction conflict".into()),
        )
        .unwrap();
        assert!(extracted.join("Folder/Empty").path().is_dir());
        assert_eq!(
            fs::read(extracted.join("Folder/file.txt").path()).unwrap(),
            b"SFTP archive contents"
        );
        assert_eq!(
            fs::read(extracted.join("file.txt").path()).unwrap(),
            b"SFTP archive contents"
        );
        assert_eq!(fs::read_dir(extracted.path()).unwrap().count(), 2);
    }
    assert_eq!(fs::read(file.path()).unwrap(), b"SFTP archive contents");
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 7);
    fs::remove_dir_all(parent.path()).unwrap();
}

// Run with scripts/test-sftp.sh: a disposable loopback-only OpenSSH server
// and generated keys, without using the user's real known_hosts or keys.
#[test]
#[ignore = "requires disposable OpenSSH server; run scripts/test-sftp.sh"]
fn openssh_round_trip_and_host_verification() {
    let root =
        PathBuf::from(std::env::var_os("PAWCOMMANDER_TEST_SFTP_ROOT").expect("test server root"));
    #[cfg(target_os = "macos")]
    check_sftp_connection_errors(&root);
    let expected = Endpoint {
        host: std::env::var("PAWCOMMANDER_TEST_SFTP_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("PAWCOMMANDER_TEST_SFTP_PORT")
            .unwrap()
            .parse()
            .unwrap(),
        user: std::env::var("PAWCOMMANDER_TEST_SFTP_USER").unwrap(),
        key: Some(root.join("client")),
        jump: None,
    };
    let home = tempfile::tempdir_in(&root).unwrap();
    let ssh = home.path().join(".ssh");
    fs::create_dir(&ssh).unwrap();
    fs::write(
        ssh.join("config"),
        format!(
            "Host fixture\nHostName {}\nUser {}\nPort {}\nIdentityFile ~/.ssh/unused\nIdentityFile \"{}\"\nStrictHostKeyChecking no\n",
            expected.host, expected.user, expected.port, root.join("client").display()
        ),
    )
    .unwrap();
    let imported = crate::ssh_config::load_config(home.path(), "fixture", "unused").unwrap();
    assert!(imported.matched);
    assert_eq!(imported.identities.len(), 2);
    assert_eq!(imported.unsupported[0].keyword, "StrictHostKeyChecking");
    let endpoint = Endpoint {
        host: imported.host,
        port: imported.port,
        user: imported.user,
        key: Some(imported.identities[1].clone()),
        jump: None,
    };
    assert_eq!(endpoint, expected);
    let known = root.join("known_hosts");
    let empty = root.join("empty_hosts");
    fs::write(&empty, "").unwrap();
    assert_eq!(
        connect(&endpoint, "", "test-passphrase", &empty)
            .err()
            .unwrap(),
        crate::i18n::text!(
            "Ключ сервера SSH не подтверждён. Откройте форму SFTP и проверьте отпечаток."
        ),
    );
    let mismatch = root.join("mismatch_hosts");
    let other_key = fs::read_to_string(root.join("other.pub")).unwrap();
    fs::write(
        &mismatch,
        format!("[{}]:{} {}", endpoint.host, endpoint.port, other_key),
    )
    .unwrap();
    assert!(connect(&endpoint, "", "test-passphrase", &mismatch)
        .err()
        .unwrap()
        .starts_with(crate::i18n::text!("Ключ сервера SSH изменился.")));
    assert!(connect(&endpoint, "", "wrong-passphrase", &known).is_err());
    let memory_key = fs::read_to_string(root.join("client")).unwrap();
    let absent_key = Endpoint {
        key: Some(root.join("does-not-exist")),
        ..endpoint.clone()
    };
    assert!(connect_with_key(
        &absent_key,
        "",
        "test-passphrase",
        &known,
        Some(&memory_key)
    )
    .unwrap()
    .authenticated());
    assert!(connect_with_key(&absent_key, "", "wrong-phrase", &known, Some(&memory_key)).is_err());
    let remote_root = Location::Sftp {
        endpoint: endpoint.clone(),
        path: root.join("remote"),
    };
    assert!(read_automatic_directory(&remote_root, &|| false)
        .unwrap()
        .is_none());
    visit_address_directory(&remote_root, true, &|| false, |_| {
        panic!("completion must not authenticate to browse an unknown endpoint")
    })
    .unwrap();
    assert!(!sessions().lock().unwrap().contains_key(&endpoint));
    let session = connect(&endpoint, "", "test-passphrase", &known).unwrap();
    sessions()
        .lock()
        .unwrap()
        .insert(endpoint.clone(), session.into());
    let local_root = Location::Local(root.join("local"));
    fs::create_dir(local_root.path()).unwrap();
    fs::create_dir(remote_root.path()).unwrap();
    #[cfg(target_os = "macos")]
    check_sftp_initial_folder(&endpoint, &root);
    let duplicate = create_directory(&remote_root.parent().unwrap(), "remote").unwrap_err();
    let TransferError::Failed(crate::file_error::Failure::Mutation(duplicate)) = duplicate else {
        panic!("expected a typed SFTP directory conflict: {duplicate:?}");
    };
    assert!(matches!(
        duplicate.as_ref(),
        crate::file_error::MutationError::CreateSftp { destination, source }
            if destination == &remote_root && matches!(source.code(), ssh2::ErrorCode::SFTP(_))
    ));
    let missing = remote_root.join("nonexistent-listing-test");
    let failure = read_directory(&missing, &|| false).unwrap_err();
    let TransferError::Failed(crate::file_error::Failure::Listing(failure)) = failure else {
        panic!("expected a typed failure for the absent SFTP directory: {failure:?}");
    };
    assert_eq!(failure.location, missing);
    assert!(matches!(
        failure.kind,
        crate::file_error::ListingErrorKind::OpenSftp(source)
            if source.code() == ssh2::ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_NO_SUCH_FILE)
    ));
    check_sftp_deletion_results(&remote_root);
    #[cfg(target_os = "macos")]
    check_sftp_access_errors(&remote_root);
    #[cfg(target_os = "macos")]
    check_sftp_preview_errors(&remote_root);
    #[cfg(target_os = "macos")]
    check_sftp_editor(
        &local_root,
        &remote_root,
        std::env::var("PAWCOMMANDER_TEST_SFTP_METADATA_DENIED").as_deref() == Ok("1"),
    );
    #[cfg(unix)]
    if std::env::var("PAWCOMMANDER_TEST_SFTP_METADATA_DENIED").as_deref() == Ok("1") {
        super::metadata::server_rejected(&local_root, &remote_root);
        super::permissions::check(&remote_root, true);
        sessions().lock().unwrap().remove(&endpoint);
        return;
    }
    #[cfg(unix)]
    super::permissions::check(&remote_root, false);
    #[cfg(unix)]
    crate::search::check_sftp_search(&remote_root);
    super::undo::sftp_undo(&remote_root);
    check_sftp_archive_round_trip(&remote_root);
    let refresh_file = remote_root.join("external.txt");
    fs::write(refresh_file.path(), b"external edit").unwrap();
    let accessed = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    let modified = UNIX_EPOCH + Duration::from_secs(1_650_000_000);
    fs::OpenOptions::new()
        .write(true)
        .open(refresh_file.path())
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_accessed(accessed)
                .set_modified(modified),
        )
        .unwrap();
    #[cfg(unix)]
    let expected_owner = {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        fs::set_permissions(refresh_file.path(), fs::Permissions::from_mode(0o640)).unwrap();
        fs::metadata(refresh_file.path()).unwrap().uid()
    };
    let entries = read_automatic_directory(&remote_root, &|| false)
        .unwrap()
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, refresh_file);
    assert_eq!(entries[0].size, 13);
    assert_eq!(entries[0].modified, Some(modified));
    assert_eq!(entries[0].metadata.accessed, Some(accessed));
    assert_eq!(entries[0].metadata.created, None);
    #[cfg(unix)]
    {
        assert_eq!(entries[0].metadata.mode, Some(0o640));
        assert_eq!(entries[0].metadata.owner_id, Some(expected_owner));
        let alias = remote_root.with_path(root.join("remote-alias"));
        std::os::unix::fs::symlink(remote_root.path(), alias.path()).unwrap();
        let error = transfer_with_conflicts(
            std::slice::from_ref(&refresh_file),
            &alias,
            TransferKind::Move,
            &mut Progress::default(),
            |_| panic!("same-folder moves through an alias must fail before conflicts"),
            |_, _| panic!("same-folder moves must preserve the original entry"),
        )
        .unwrap_err();
        use crate::file_error::{Failure, TransferPlanError};
        assert!(error.summary.preparing);
        assert!(
            matches!(error.error, TransferError::Failed(Failure::TransferPlan(reason))
            if matches!(reason.as_ref(), TransferPlanError::SameFolder { source, destination }
                if source == &refresh_file && destination == &alias))
        );
        assert_eq!(fs::read(refresh_file.path()).unwrap(), b"external edit");

        let local_same_name = local_root.join("external.txt");
        fs::write(local_same_name.path(), b"local source with the same name").unwrap();
        let error = transfer_with_conflicts(
            &[local_same_name.clone(), refresh_file.clone()],
            &remote_root,
            TransferKind::Copy,
            &mut Progress::default(),
            |_| panic!("duplicate destination must fail before conflicts"),
            |_, _| panic!("duplicate destination must fail before publication"),
        )
        .unwrap_err();
        assert!(error.summary.preparing && error.summary.completed == 0);
        assert!(
            matches!(error.error, TransferError::Failed(Failure::TransferPlan(reason))
            if matches!(reason.as_ref(), TransferPlanError::DuplicateTarget { first, second, target }
                if first == &local_same_name && second == &refresh_file && target == &refresh_file))
        );
        assert_eq!(fs::read(refresh_file.path()).unwrap(), b"external edit");
        assert_eq!(
            fs::read(local_same_name.path()).unwrap(),
            b"local source with the same name"
        );
        fs::remove_file(local_same_name.path()).unwrap();
    }
    fs::remove_file(refresh_file.path()).unwrap();
    assert!(read_automatic_directory(&remote_root, &|| false)
        .unwrap()
        .unwrap()
        .is_empty());
    let completion_folder = remote_root.join("Žlutá папка #📂");
    fs::create_dir(completion_folder.path()).unwrap();
    let actual = fs::read_dir(remote_root.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let completion_folder = remote_root.with_path(actual.path());
    let mut completion = crate::address::Completion::default();
    completion.edited = true;
    let query = format!("{}/z\u{30c}l", remote_root.display());
    let ctx = eframe::egui::Context::default();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let suggestions = completion.suggestions(&query, &local_root, &ctx);
        if let Some(suggestion) = suggestions.first() {
            assert_eq!(suggestion.path, completion_folder);
            assert_eq!(suggestion.name, actual.file_name().to_str().unwrap());
            completion.remember_completion(&suggestion.path);
            let tab_address = format!("{}/", suggestion.path.display());
            assert_eq!(
                completion.resolve(&tab_address, &local_root).unwrap(),
                completion_folder
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "expected SFTP completion"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    fs::write(refresh_file.path(), b"another entry").unwrap();
    let mut tree_children = Vec::new();
    visit_connected_directory(&remote_root, &|| false, |entry| {
        if entry.is_dir {
            tree_children.push(entry.path);
        }
    })
    .unwrap();
    assert_eq!(tree_children, std::slice::from_ref(&completion_folder));
    let other_profile = Location::Sftp {
        endpoint: absent_key,
        path: remote_root.path().into(),
    };
    assert!(visit_connected_directory(&other_profile, &|| false, |_| {
        panic!("the tree must use the exact SFTP profile")
    })
    .is_err());
    let visited = std::cell::Cell::new(0);
    visit_address_directory(&remote_root, false, &|| visited.get() == 1, |entry| {
        assert!(entry.path == completion_folder || entry.path == refresh_file);
        visited.set(visited.get() + 1);
    })
    .unwrap();
    assert_eq!(visited.get(), 1);
    fs::remove_file(refresh_file.path()).unwrap();
    fs::remove_dir(completion_folder.path()).unwrap();
    let source = create_directory(&local_root, "папка ' [test]").unwrap();
    fs::write(source.path().join("hello world.txt"), "Привет, SFTP!\n").unwrap();
    fs::write(source.path().join(".hidden"), "hidden").unwrap();
    fs::write(source.path().join("empty"), "").unwrap();
    fs::create_dir(source.path().join("sub")).unwrap();
    fs::write(source.path().join("sub/large.bin"), vec![0xab; 512 * 1024]).unwrap();
    // A stopped worker reports its own remote staging path without scanning
    // or deleting unrelated SFTP entries.
    let recovery_dir = local_root.path().join("recovery");
    let pending = crate::recovery::begin(&recovery_dir, "upload", "fixture", 1).unwrap();
    let mut interrupted_staging = None;
    let interrupted = pending
        .run(|| {
            with_staging(&remote_root.join("unpublished"), |staging, preserve| {
                *preserve = true;
                fs::write(staging.path().join("payload"), b"partial upload").unwrap();
                interrupted_staging = Some(staging.clone());
                Err("injected interruption".into())
            })
        })
        .unwrap();
    assert!(interrupted.is_err());
    drop(pending);
    let report = crate::recovery::scan(&recovery_dir);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.unfinished.len(), 1);
    assert_eq!(report.unfinished[0].staging.len(), 1);
    assert!(report.unfinished[0].planned_staging.is_empty());
    let staged = interrupted_staging.unwrap();
    assert_eq!(report.unfinished[0].staging[0].location, staged.display());
    assert!(report.unfinished[0].staging[0].identity.is_none());
    assert_eq!(
        fs::read(staged.path().join("payload")).unwrap(),
        b"partial upload"
    );
    fs::remove_dir_all(staged.path()).unwrap();
    if std::env::var("PAWCOMMANDER_TEST_DISABLE_STATVFS").as_deref() == Ok("1") {
        use crate::file_error::{Failure, ProbeAction, SpaceError};

        let target = remote_root.join(source.name());
        let error = staged_copy(&source, &target).unwrap_err();
        assert!(error.contains("fstatvfs@openssh.com"), "{error}");
        assert!(!target.path().exists());
        assert!(read_directory(&remote_root, &|| false).unwrap().is_empty());
        assert_eq!(fs::read(source.path().join(".hidden")).unwrap(), b"hidden");
        let mut move_progress = Progress::default();
        let error = transfer_with_conflicts(
            std::slice::from_ref(&source),
            &remote_root,
            TransferKind::Move,
            &mut move_progress,
            |_| panic!("unexpected conflict"),
            |_, _| panic!("source removed"),
        )
        .unwrap_err();
        let TransferError::Failed(Failure::Space(reason)) = &error.error else {
            panic!("expected a typed remote space error: {error:?}");
        };
        let SpaceError::SftpProbe(probe) = reason.as_ref() else {
            panic!("expected an SFTP probe error: {reason:?}");
        };
        assert_eq!(probe.directory, remote_root);
        assert_eq!(
            probe.probe.path().file_name(),
            Some(std::ffi::OsStr::new("space-probe"))
        );
        let [(ProbeAction::Query, cause)] = probe.causes.as_slice() else {
            panic!("expected the refusal and successful cleanup: {probe:?}");
        };
        assert_eq!(
            cause.code(),
            ssh2::ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_PERMISSION_DENIED)
        );
        let message = crate::i18n::transfer_failure(&error);
        assert!(message.contains("fstatvfs@openssh.com"), "{message}");
        assert!(message.contains("[SFTP(3)]"), "{message}");
        assert!(!probe.probe.path().exists());
        assert!(!target.path().exists());
        assert_eq!(fs::read(source.path().join(".hidden")).unwrap(), b"hidden");
        assert!(read_directory(&remote_root, &|| false).unwrap().is_empty());
        sessions().lock().unwrap().remove(&endpoint);
        return;
    }
    // Exercise fstatvfs on the destination filesystem before upload,
    // then exercise the remote size walk used before download/replacement.
    let expected_size = 512 * 1024 + "Привет, SFTP!\n".len() as u64 + "hidden".len() as u64;
    assert_eq!(
        TransferSourceManifest::capture(&source).unwrap().bytes,
        expected_size
    );
    super::super::transfer::with_staging_typed(
        &remote_root.join(source.name()),
        None,
        |staging, _| check_copy_space(expected_size, &remote_root.join(source.name()), staging),
    )
    .unwrap();
    let (mut progress, mut view) = Progress::tracked();
    progress.set_copy_verification(true);
    assert_eq!(
        transfer_with_conflicts(
            std::slice::from_ref(&source),
            &remote_root,
            TransferKind::Copy,
            &mut progress,
            |_| panic!("unexpected conflict"),
            |_, _| {},
        )
        .unwrap()
        .completed,
        1
    );
    assert_eq!(
        view.snapshot().total_written,
        512 * 1024 + "Привет, SFTP!\n".len() as u64 + "hidden".len() as u64
    );
    let entries = read_directory(&remote_root, &|| false).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, source.name());
    assert!(entries[0].is_dir);
    let uploaded = entries[0].path.clone();
    assert_eq!(
        TransferSourceManifest::capture(&uploaded).unwrap().bytes,
        expected_size
    );
    check_copy_space(
        expected_size,
        &local_root.join("download-preflight"),
        &local_root,
    )
    .unwrap();
    assert_eq!(read_directory(&uploaded, &|| false).unwrap().len(), 4);
    assert!(transfer(
        std::slice::from_ref(&source),
        &remote_root,
        TransferKind::Copy
    )
    .is_err());
    let downloads = create_directory(&local_root, "downloads").unwrap();
    let mut verified_download = Progress::default();
    verified_download.set_copy_verification(true);
    transfer_with_conflicts(
        std::slice::from_ref(&uploaded),
        &downloads,
        TransferKind::Copy,
        &mut verified_download,
        |_| panic!("unexpected conflict"),
        |_, _| panic!("copy"),
    )
    .unwrap();
    assert_eq!(
        fs::read(downloads.join(source.name()).path().join("sub/large.bin")).unwrap(),
        vec![0xab; 512 * 1024]
    );
    assert_eq!(
        fs::read_to_string(downloads.join(source.name()).path().join("hello world.txt")).unwrap(),
        "Привет, SFTP!\n"
    );
    let file_source = source.join("hello world.txt");
    let replace = |conflict: Conflict| {
        assert!(conflict.can_replace());
        assert!(conflict.source.size.is_some());
        assert!(conflict.target.modified.is_some());
        Ok(Answer {
            decision: Decision::Replace,
            apply_to_group: false,
        })
    };
    transfer_with_conflicts(
        std::slice::from_ref(&file_source),
        &uploaded,
        TransferKind::Copy,
        &mut Progress::default(),
        replace,
        |_, _| {},
    )
    .unwrap();
    assert_eq!(
        preview_data(
            &uploaded.join("hello world.txt"),
            5,
            &Cancellation::default()
        )
        .unwrap()
        .1
        .len(),
        6
    );
    assert_eq!(
        preview_data(
            &uploaded.join("hello world.txt"),
            1024,
            &Cancellation::default()
        )
        .unwrap()
        .1,
        "Привет, SFTP!\n".as_bytes()
    );
    {
        use std::io::{Read, Seek, SeekFrom};
        let location = uploaded.join("hello world.txt");
        let mut file = crate::vfs::open_preview_stream(&location).unwrap();
        file.seek(SeekFrom::Start("Привет, ".len() as u64)).unwrap();
        let mut tail = String::new();
        file.read_to_string(&mut tail).unwrap();
        assert_eq!(tail, "SFTP!\n");
    }
    {
        let text = "ordinary remote line\r\n".repeat(4096) + "REMOTE PREVIEW MARKER\r\n";
        let mut utf16 = vec![0xff, 0xfe];
        utf16.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        for (name, bytes) in [
            ("large UTF-8 preview.txt", text.into_bytes()),
            ("large UTF-16 preview.txt", utf16),
        ] {
            let location = uploaded.join(name);
            fs::write(location.path(), &bytes).unwrap();
            crate::preview::check_sftp_large_text(&location);
            assert_eq!(fs::read(location.path()).unwrap(), bytes);
            fs::remove_file(location.path()).unwrap();
        }
    }
    {
        let location = uploaded.join("binary preview.bin");
        let marker = b"\xde\xad\xbe\xef";
        let offset = 65535;
        let mut bytes = vec![0; 131_077];
        bytes[offset..offset + marker.len()].copy_from_slice(marker);
        fs::write(location.path(), &bytes).unwrap();
        crate::preview::check_sftp_hex(&location, offset as u64, marker);
        assert_eq!(fs::read(location.path()).unwrap(), bytes);
        fs::remove_file(location.path()).unwrap();
    }
    #[cfg(target_os = "macos")]
    {
        crate::preview::check_sftp_image_metadata(&uploaded.join("Photo with EXIF.jpg"));
        let checksum_directory = uploaded.join("checksum fixtures");
        fs::create_dir(checksum_directory.path()).unwrap();
        super::super::checksum::checksum_round_trip(&checksum_directory);
        fs::remove_dir_all(checksum_directory.path()).unwrap();
        let local_compare = tempfile::tempdir().unwrap();
        let remote_compare = uploaded.join("comparison fixtures");
        let comparison_folders = [
            local_compare.path().to_path_buf().into(),
            remote_compare.clone(),
        ];
        super::super::compare::comparison_round_trip(&comparison_folders);
        super::super::compare::comparison_sftp_denials(&comparison_folders);
        super::super::file_compare::file_comparison_round_trip(&[
            local_compare.path().join("left compare.txt").into(),
            remote_compare.join("right compare.txt"),
        ]);
        fs::remove_dir_all(remote_compare.path()).unwrap();
        let location = uploaded.join("hello world.txt");
        let mut destination = Vec::new();
        let cancel = crate::operations::Cancellation::default();
        assert!(cache_media_preview(&location, &mut destination, 5, &cancel).is_err());
        assert!(destination.is_empty());
        cache_media_preview(&location, &mut destination, 1024, &cancel).unwrap();
        assert_eq!(destination, "Привет, SFTP!\n".as_bytes());
        let mut checks = 0;
        let cancel = crate::operations::Cancellation::at_checkpoint(move || {
            checks += 1;
            checks == 3
        });
        destination.clear();
        assert!(matches!(
            cache_media_preview(&location, &mut destination, 1024, &cancel),
            Err(TransferError::Cancelled)
        ));
        assert!(destination.is_empty());
    }
    fs::write(file_source.path(), "replacement").unwrap();
    transfer_with_conflicts(
        std::slice::from_ref(&file_source),
        &uploaded,
        TransferKind::Copy,
        &mut Progress::default(),
        replace,
        |_, _| {},
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(uploaded.join("hello world.txt").path()).unwrap(),
        "replacement"
    );
    transfer_with_conflicts(
        std::slice::from_ref(&file_source),
        &uploaded,
        TransferKind::Copy,
        &mut Progress::default(),
        |_| {
            Ok(Answer {
                decision: Decision::KeepBoth,
                apply_to_group: false,
            })
        },
        |_, _| {},
    )
    .unwrap();
    let remote_source = uploaded.join("hello world (2).txt");
    let remote_target = remote_root.join("hello world (2).txt");
    assert_eq!(fs::read(remote_source.path()).unwrap(), b"replacement");
    fs::write(remote_target.path(), "previous remote file").unwrap();
    transfer_with_conflicts(
        std::slice::from_ref(&remote_source),
        &remote_root,
        TransferKind::Move,
        &mut Progress::default(),
        replace,
        |_, _| {},
    )
    .unwrap();
    assert!(!remote_source.path().exists());
    assert_eq!(fs::read(remote_target.path()).unwrap(), b"replacement");
    assert!(transfer_with_conflicts(
        std::slice::from_ref(&source),
        &remote_root,
        TransferKind::Copy,
        &mut Progress::default(),
        |conflict| {
            assert!(!conflict.can_replace());
            Ok(Answer {
                decision: Decision::Replace,
                apply_to_group: false,
            })
        },
        |_, _| {},
    )
    .is_err());
    let disposable = create_directory(&remote_root, "delete-me").unwrap();
    fs::write(disposable.path().join("file"), "delete").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(source.path(), disposable.path().join("link")).unwrap();
    fs::create_dir(disposable.path().join("nested")).unwrap();
    fs::write(disposable.path().join("nested/child"), "nested delete").unwrap();
    let mut deletion = Progress::default();
    trash_entries_with_progress(std::slice::from_ref(&disposable), &mut deletion).unwrap();
    let record = deletion
        .results
        .finish("delete".into(), 1, &Ok("removed".into()))
        .unwrap();
    assert!(record.items[0].descendants_enumerated);
    assert_eq!(record.items.len(), if cfg!(unix) { 5 } else { 4 });
    assert!(record
        .items
        .iter()
        .all(|row| row.outcome == crate::result_history::Outcome::Completed));
    assert!(record
        .items
        .iter()
        .any(|row| row.source == disposable.join("nested/child").display()));
    assert!(!disposable.path().exists());
    assert!(source.path().exists());
    let renamed = rename_entry(&uploaded, "renamed").unwrap();
    let conflict = create_directory(&remote_root, "conflict").unwrap();
    assert!(rename_entry(&renamed, "conflict").is_err());
    assert!(remote(&conflict)
        .unwrap()
        .stat(conflict.path())
        .unwrap()
        .is_dir());
    // Direct no-replace publication must also reject an existing target.
    let file = renamed.join("hello world.txt");
    assert!(staged_copy(&file, &renamed.join(".hidden")).is_err());
    let occupied = renamed.join(".hidden");
    let ambiguous = rename_for_publication(&file, &occupied);
    assert!(matches!(
        ambiguous,
        Err(TransferError::PublicationUnconfirmed(crate::file_error::Failure::Mutation(reason)))
            if matches!(reason.as_ref(), crate::file_error::MutationError::RenameSftp {
                from, to, source
            } if from == &file && to == &occupied && matches!(source.code(), ssh2::ErrorCode::SFTP(_)))
    ));
    assert_eq!(fs::read_to_string(file.path()).unwrap(), "replacement");
    assert_eq!(
        fs::read_to_string(renamed.path().join(".hidden")).unwrap(),
        "hidden"
    );
    let sub = create_directory(&remote_root, "destination").unwrap();
    transfer(std::slice::from_ref(&renamed), &sub, TransferKind::Move).unwrap();
    let moved = sub.join("renamed");
    assert!(transfer(std::slice::from_ref(&moved), &moved, TransferKind::Copy).is_err());
    assert!(remote(&moved).unwrap().stat(moved.path()).unwrap().is_dir());
    // Remote -> remote copy and refusal to follow symlinks in transfer trees.
    let mut verified_remote = Progress::default();
    verified_remote.set_copy_verification(true);
    transfer_with_conflicts(
        std::slice::from_ref(&moved),
        &conflict,
        TransferKind::Copy,
        &mut verified_remote,
        |_| panic!("unexpected conflict"),
        |_, _| panic!("copy"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source.path(), remote_root.path().join("alias")).unwrap();
        let link = remote_root.join("alias");
        assert!(read_directory(&remote_root, &|| false)
            .unwrap()
            .iter()
            .any(|e| e.is_symlink && e.is_dir));
        assert!(transfer(&[link], &downloads, TransferKind::Copy).is_err());
        assert!(!downloads.path().join("alias").exists());
        std::os::unix::fs::symlink(source.path(), source.path().join("loop")).unwrap();
        assert!(transfer(
            std::slice::from_ref(&source),
            &remote_root,
            TransferKind::Copy
        )
        .is_err());
        assert!(!remote_root.join(source.name()).path().exists());
    }
    assert!(fs::read_dir(remote_root.path()).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pawcommander-sftp-")
    }));
    assert!(fs::read_dir(downloads.path()).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pawcommander-transfer-")
    }));
    check_sftp_cancellation(&local_root, &remote_root);
    super::pause::check_copy_pause(
        &local_root,
        &remote_root,
        "pause-upload",
        super::pause::PauseAction::Resume,
        true,
    );
    super::pause::check_copy_pause(
        &remote_root,
        &local_root,
        "pause-download",
        super::pause::PauseAction::Stop,
        false,
    );
    super::bandwidth::check_transfer_speed(&local_root, &remote_root, "upload", false);
    super::bandwidth::check_transfer_speed(&remote_root, &local_root, "download", true);
    check_sftp_retry(&local_root, &remote_root);
    check_cross_connection_partial_move(&local_root, &remote_root);
    check_sftp_copy_hash_verification(&local_root, &remote_root);
    check_sftp_source_changes(&local_root, &remote_root);
    check_sftp_source_change_cleanup_at_depth_limit(&local_root, &remote_root);
    #[cfg(unix)]
    {
        super::metadata::round_trip(&local_root, &remote_root);
        super::metadata::partially_preserved(&local_root, &remote_root);
        super::metadata::cancelled_after_attributes(&local_root, &remote_root);
    }
    #[cfg(unix)]
    check_sftp_unsupported_names(&local_root, &remote_root);
    #[cfg(target_os = "macos")]
    {
        check_promised_sftp_download(&local_root, &remote_root);
        check_changed_promised_source_after_preflight(&local_root, &remote_root);
        crate::properties::check_sftp_properties(&remote_root);
        crate::vfs::file_templates::check_sftp(&remote_root);
        super::super::sync::sync_round_trip(&remote_root);
        super::super::sync::two_way_round_trip(&remote_root);
    }
    sessions().lock().unwrap().remove(&endpoint);
}
