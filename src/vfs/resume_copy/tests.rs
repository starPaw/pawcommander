use super::*;
use crate::conflict::{Answer, Decision};
use crate::fs_ops::TransferKind;
use crate::vfs::transfer_with_conflicts;
use std::io::{Seek, SeekFrom, Write};

fn staged_payload(folder: &Path) -> Option<PathBuf> {
    fs::read_dir(folder)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".pawcommander-")
        })
        .map(|entry| entry.path().join("payload"))
}

fn first_copy(
    source: &Location,
    destination: &Location,
    recovery: &Path,
    checkpoint: impl FnMut() -> bool + Send + 'static,
) -> Result<(), String> {
    let (mut progress, _) = Progress::tracked_transfer();
    progress.resume_directory = Some(recovery.to_owned());
    progress.cancellation().set_checkpoint(checkpoint);
    transfer_with_conflicts(
        std::slice::from_ref(source),
        destination,
        TransferKind::Copy,
        &mut progress,
        |_| {
            Ok(Answer {
                decision: Decision::Replace,
                apply_to_group: false,
            })
        },
        |_, _| {},
    )
    .map(|_| ())
}

fn continued(recovery: &Path, partial: &Path, source: &Path, target: &Path, bytes: &[u8]) {
    let offset = partial.metadata().unwrap().len();
    assert!((1..bytes.len() as u64).contains(&offset));
    let (offers, errors) = scan(recovery);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(offers.len(), 1);
    let (mut progress, mut view) = Progress::tracked_transfer();
    let result = resume(recovery, &offers[0].id, &mut progress).unwrap();
    assert!(result.contains(&offset.to_string()), "{result}");
    assert_eq!(view.snapshot().total_written, bytes.len() as u64 - offset);
    assert_eq!(fs::read(source).unwrap(), bytes);
    assert_eq!(fs::read(target).unwrap(), bytes);
    assert!(!partial.parent().unwrap().exists());
    assert!(scan(recovery).0.is_empty());
    println!(
        "Continued at byte {offset}, wrote {} bytes and verified the destination",
        view.snapshot().total_written
    );
}

#[test]
fn retained_copy_restarts_from_checked_offset_and_never_retries_changed_data() {
    for change in [
        None,
        Some("source"),
        Some("target"),
        Some("payload"),
        Some("parked"),
        Some("published"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let recovery = root.join("recovery");
        let source = root.join("source.bin");
        let output = root.join("destination");
        let target = output.join("source.bin");
        fs::create_dir(&output).unwrap();
        let bytes: Vec<_> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
        fs::write(&source, &bytes).unwrap();
        fs::write(&target, b"previous destination").unwrap();
        let output_at_checkpoint = output.clone();
        let interrupted = std::panic::catch_unwind(|| {
            let _ = first_copy(
                &source.clone().into(),
                &output.clone().into(),
                &recovery,
                move || {
                    if staged_payload(&output_at_checkpoint)
                        .and_then(|path| path.metadata().ok())
                        .is_some_and(|metadata| metadata.len() >= 128 * 1024)
                    {
                        std::panic::panic_any("interrupted test copy");
                    }
                    false
                },
            );
        });
        assert!(interrupted.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"previous destination");
        let partial = staged_payload(&output).unwrap();
        let Some(change) = change else {
            continued(&recovery, &partial, &source, &target, &bytes);
            continue;
        };
        if matches!(change, "parked" | "published") {
            let stage = partial.parent().unwrap();
            fs::rename(&target, stage.join("previous")).unwrap();
            if change == "parked" {
                continued(&recovery, &partial, &source, &target, &bytes);
            } else {
                fs::write(&partial, &bytes).unwrap();
                fs::rename(&partial, &target).unwrap();
                let (offers, errors) = scan(&recovery);
                assert!(errors.is_empty());
                let (mut progress, mut view) = Progress::tracked_transfer();
                assert_eq!(
                    resume(&recovery, &offers[0].id, &mut progress).unwrap(),
                    text("В назначении подтверждена полная копия.")
                );
                assert_eq!(view.snapshot().total_written, 0);
                assert_eq!(fs::read(&target).unwrap(), bytes);
                assert!(!stage.exists());
                assert!(scan(&recovery).0.is_empty());
            }
            continue;
        }
        let changed = match change {
            "source" => &source,
            "target" => &target,
            _ => &partial,
        };
        let times = changed.metadata().unwrap();
        let mut file = fs::OpenOptions::new().write(true).open(changed).unwrap();
        file.seek(SeekFrom::Start(2)).unwrap();
        file.write_all(b"changed").unwrap();
        file.set_times(fs::FileTimes::new().set_modified(times.modified().unwrap()))
            .unwrap();
        let snapshot = [&source, &target, &partial].map(|path| fs::read(path).unwrap());
        let (offers, errors) = scan(&recovery);
        assert!(errors.is_empty());
        assert_eq!(offers.len(), 1);
        let (mut progress, mut view) = Progress::tracked_transfer();
        assert!(
            resume(&recovery, &offers[0].id, &mut progress).is_err(),
            "{change}"
        );
        assert_eq!(view.snapshot().total_written, 0, "{change}");
        assert_eq!(
            snapshot,
            [&source, &target, &partial].map(|path| fs::read(path).unwrap())
        );
        assert_eq!(scan(&recovery).0.len(), 1);
    }
}

pub(crate) fn check_sftp(root: &Path, remote_root: &Location) {
    use std::net::{Shutdown, TcpStream};
    use std::os::fd::{AsRawFd, BorrowedFd};

    for upload in [true, false] {
        let name = if upload {
            "resume-upload"
        } else {
            "resume-download"
        };
        let remote = remote_root.join(name);
        let local: Location = root.join(name).into();
        let recovery = root.join(format!("recovery-{name}"));
        fs::create_dir(local.path()).unwrap();
        fs::create_dir(remote.path()).unwrap();
        let (input, output) = if upload {
            (&local, &remote)
        } else {
            (&remote, &local)
        };
        let source = input.join("copy.bin");
        let target = output.join("copy.bin");
        let bytes: Vec<_> = (0..1024 * 1024).map(|i| (i % 241) as u8).collect();
        fs::write(source.path(), &bytes).unwrap();
        fs::write(target.path(), b"old file").unwrap();

        drop(super::remote(&remote).unwrap());
        let endpoint = remote.sftp_endpoint().unwrap();
        let cached = super::super::sftp::sessions().lock().unwrap()[endpoint].clone();
        // SAFETY: cached owns the live descriptor until the duplicate is made.
        let fd = unsafe { BorrowedFd::borrow_raw(cached.as_raw_fd()) };
        let socket = TcpStream::from(fd.try_clone_to_owned().unwrap());
        let output_path = output.path().to_owned();
        let mut disconnected = false;
        let result = first_copy(&source, output, &recovery, move || {
            if !disconnected
                && staged_payload(&output_path)
                    .and_then(|path| path.metadata().ok())
                    .is_some_and(|metadata| metadata.len() >= 128 * 1024)
            {
                socket.shutdown(Shutdown::Both).unwrap();
                disconnected = true;
            }
            false
        });
        let error = result.unwrap_err();
        assert_eq!(fs::read(target.path()).unwrap(), b"old file");
        let partial = staged_payload(output.path()).unwrap_or_else(|| panic!("{name}: {error}"));
        continued(&recovery, &partial, source.path(), target.path(), &bytes);
        assert!(!std::sync::Arc::ptr_eq(
            &cached,
            &super::super::sftp::sessions().lock().unwrap()[endpoint]
        ));
    }
    check_sftp_folders(root, remote_root);
}

const HEAD: &[u8] = b"already completed child";

fn setup_folder(source: &Path, tail: &[u8]) {
    fs::create_dir_all(source.join("z-child/empty")).unwrap();
    fs::write(source.join("a-finished"), HEAD).unwrap();
    fs::write(source.join("z-child/large"), tail).unwrap();
}

fn tail_progress(destination: &Path) -> Option<u64> {
    staged_payload(destination)?
        .join("z-child/large")
        .metadata()
        .ok()
        .map(|m| m.len())
}

fn checked_folder(recovery: &Path, stage: &Path, source: &Path, target: &Path, bytes: &[u8]) {
    let suffix_offset = fs::metadata(stage.join("z-child/large")).unwrap().len();
    assert!((1..bytes.len() as u64).contains(&suffix_offset));
    assert_eq!(fs::read(stage.join("a-finished")).unwrap(), HEAD);
    assert!(!target.exists());
    let (offers, errors) = scan(recovery);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(offers.len(), 1);
    let (mut progress, mut view) = Progress::tracked_transfer();
    let message = resume(recovery, &offers[0].id, &mut progress).unwrap();
    let reused = suffix_offset + HEAD.len() as u64;
    assert!(message.contains(&reused.to_string()), "{message}");
    assert_eq!(
        view.snapshot().total_written,
        bytes.len() as u64 - suffix_offset
    );
    for root in [source, target] {
        assert_eq!(fs::read(root.join("a-finished")).unwrap(), HEAD);
        assert_eq!(fs::read(root.join("z-child/large")).unwrap(), bytes);
        assert!(root.join("z-child/empty").is_dir());
    }
    assert!(scan(recovery).0.is_empty());
    assert!(!stage.parent().unwrap().exists());
    println!(
        "Reused {reused} staged bytes, wrote only {} remaining folder bytes",
        view.snapshot().total_written
    );
}

#[test]
fn interrupted_folder_reuses_verified_children_and_refuses_changed_trees() {
    let tail: Vec<_> = (0..1024 * 1024).map(|i| (i % 233) as u8).collect();
    for change in [
        None,
        Some("finished"),
        Some("prefix"),
        Some("foreign"),
        Some("source"),
        Some("target"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let recovery = root.join("recovery");
        let source = root.join("tree");
        let output = root.join("destination");
        let target = output.join("tree");
        fs::create_dir(&output).unwrap();
        setup_folder(&source, &tail);
        let local_name = "b-local-file ‘файл’\n";
        fs::write(source.join(local_name), b"native name").unwrap();
        std::os::unix::fs::symlink(Path::new("missing"), source.join("c-link")).unwrap();
        let checkpoint_directory = output.clone();
        let interrupted = std::panic::catch_unwind(|| {
            let _ = first_copy(
                &source.clone().into(),
                &output.clone().into(),
                &recovery,
                move || {
                    if tail_progress(&checkpoint_directory).is_some_and(|len| len >= 128 * 1024) {
                        std::panic::panic_any("interrupted folder copy");
                    }
                    false
                },
            );
        });
        assert!(interrupted.is_err());
        let stage = staged_payload(&output).unwrap();
        let Some(change) = change else {
            let saved = fs::metadata(stage.join("z-child/large")).unwrap().len()
                + HEAD.len() as u64
                + b"native name".len() as u64;
            let (offers, errors) = scan(&recovery);
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(offers.len(), 1);
            let (mut progress, mut view) = Progress::tracked_transfer();
            let message = resume(&recovery, &offers[0].id, &mut progress).unwrap();
            assert!(message.contains(&saved.to_string()), "{message}");
            assert_eq!(
                view.snapshot().total_written,
                tail.len() as u64 + HEAD.len() as u64 + b"native name".len() as u64 - saved
            );
            assert_eq!(fs::read(target.join("a-finished")).unwrap(), HEAD);
            assert_eq!(fs::read(target.join("z-child/large")).unwrap(), tail);
            assert_eq!(fs::read(target.join(local_name)).unwrap(), b"native name");
            assert_eq!(
                fs::read_link(target.join("c-link")).unwrap(),
                Path::new("missing")
            );
            assert!(target.join("z-child/empty").is_dir());
            assert!(!stage.parent().unwrap().exists() && scan(&recovery).0.is_empty());
            continue;
        };
        let changed = match change {
            "finished" => stage.join("a-finished"),
            "prefix" => stage.join("z-child/large"),
            "source" => source.join("a-finished"),
            "target" => target.clone(),
            _ => stage.join("z-child/foreign"),
        };
        if changed.exists() {
            let mut file = fs::OpenOptions::new().write(true).open(&changed).unwrap();
            file.write_all(b"changed").unwrap();
        } else {
            fs::write(&changed, b"outside data").unwrap();
        }
        let changed_bytes = fs::read(&changed).unwrap();
        let saved = fs::read(stage.join("z-child/large")).unwrap();
        let (offers, errors) = scan(&recovery);
        assert!(errors.is_empty() && offers.len() == 1);
        let (mut progress, mut view) = Progress::tracked_transfer();
        assert!(
            resume(&recovery, &offers[0].id, &mut progress).is_err(),
            "{change}"
        );
        assert_eq!(view.snapshot().total_written, 0, "{change}");
        assert_eq!(fs::read(&changed).unwrap(), changed_bytes);
        assert_eq!(fs::read(stage.join("z-child/large")).unwrap(), saved);
        assert_eq!(scan(&recovery).0.len(), 1);
        assert_eq!(target.exists(), change == "target");
    }
}

#[test]
fn stopping_a_folder_copy_cleans_only_a_verified_draft() {
    let tail = vec![7; 768 * 1024];
    for foreign in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let recovery = root.join("recovery");
        let source = root.join("tree");
        let output = root.join("destination");
        fs::create_dir(&output).unwrap();
        setup_folder(&source, &tail);
        let observed = output.clone();
        let result = first_copy(
            &source.clone().into(),
            &output.clone().into(),
            &recovery,
            move || {
                let ready = tail_progress(&observed).is_some_and(|len| len >= 128 * 1024);
                if ready && foreign {
                    let draft = staged_payload(&observed).unwrap();
                    fs::write(draft.join("outside-data"), b"preserve me").unwrap();
                }
                ready
            },
        );
        assert_eq!(fs::read(source.join("z-child/large")).unwrap(), tail);
        assert!(!output.join("tree").exists());
        if foreign {
            assert!(result.is_err());
            let draft = staged_payload(&output).unwrap();
            assert_eq!(
                fs::read(draft.join("outside-data")).unwrap(),
                b"preserve me"
            );
            assert_eq!(scan(&recovery).0.len(), 1);
        } else {
            result.unwrap();
            assert!(staged_payload(&output).is_none());
            assert!(scan(&recovery).0.is_empty());
        }
    }
}

fn check_sftp_folders(root: &Path, remote_root: &Location) {
    use std::net::{Shutdown, TcpStream};
    use std::os::fd::{AsRawFd, BorrowedFd};
    let bytes: Vec<_> = (0..768 * 1024).map(|i| (i % 239) as u8).collect();
    for upload in [true, false] {
        let name = if upload {
            "resume-folder-upload"
        } else {
            "resume-folder-download"
        };
        let remote = remote_root.join(name);
        let local: Location = root.join(name).into();
        let recovery = root.join(format!("recovery-{name}"));
        fs::create_dir(local.path()).unwrap();
        fs::create_dir(remote.path()).unwrap();
        let (input, output) = if upload {
            (&local, &remote)
        } else {
            (&remote, &local)
        };
        let source = input.join("tree");
        let target = output.join("tree");
        setup_folder(source.path(), &bytes);
        drop(super::remote(&remote).unwrap());
        let endpoint = remote.sftp_endpoint().unwrap();
        let cached = super::super::sftp::sessions().lock().unwrap()[endpoint].clone();
        // SAFETY: cached owns the live descriptor until the duplicate is made.
        let fd = unsafe { BorrowedFd::borrow_raw(cached.as_raw_fd()) };
        let socket = TcpStream::from(fd.try_clone_to_owned().unwrap());
        let output_path = output.path().to_owned();
        let mut disconnected = false;
        let error = first_copy(&source, output, &recovery, move || {
            if !disconnected && tail_progress(&output_path).is_some_and(|len| len >= 128 * 1024) {
                socket.shutdown(Shutdown::Both).unwrap();
                disconnected = true;
            }
            false
        })
        .unwrap_err();
        let stage = staged_payload(output.path()).unwrap_or_else(|| panic!("{name}: {error}"));
        checked_folder(&recovery, &stage, source.path(), target.path(), &bytes);
        assert!(!std::sync::Arc::ptr_eq(
            &cached,
            &super::super::sftp::sessions().lock().unwrap()[endpoint]
        ));
    }
}
