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
}
