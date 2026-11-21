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
    first_transfer(
        source,
        destination,
        recovery,
        TransferKind::Copy,
        checkpoint,
    )
}

fn first_transfer(
    source: &Location,
    destination: &Location,
    recovery: &Path,
    kind: TransferKind,
    checkpoint: impl FnMut() -> bool + Send + 'static,
) -> Result<(), String> {
    let (mut progress, _) = Progress::tracked_transfer();
    progress.resume_directory = Some(recovery.to_owned());
    progress.cancellation().set_checkpoint(checkpoint);
    transfer_with_conflicts(
        std::slice::from_ref(source),
        destination,
        kind,
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
    .map_err(|failure| crate::i18n::transfer_failure(&failure))
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
    if offers[0].moving {
        assert!(!source.exists());
    } else {
        assert_eq!(fs::read(source).unwrap(), bytes);
    }
    assert_eq!(fs::read(target).unwrap(), bytes);
    assert!(!partial.parent().unwrap().exists());
    assert!(scan(recovery).0.is_empty());
    println!(
        "Continued {} at byte {offset}, wrote {} bytes and verified the destination",
        if offers[0].moving { "move" } else { "copy" },
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
                    text!("В назначении подтверждена полная копия.")
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
    for kind in [TransferKind::Copy, TransferKind::Move] {
        check_sftp_files(root, remote_root, kind);
        check_sftp_folders(root, remote_root, kind);
    }
}

fn interrupt_move(item: &TransferItem, recovery: &Path, interrupt: Phase) -> Offer {
    let (mut progress, _) = Progress::tracked_transfer();
    progress.resume_directory = Some(recovery.to_owned());
    let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cross_move::run(item, None, &mut progress, || {}, || {}, &mut |_, phase| {
            if phase == interrupt {
                std::panic::panic_any("interrupted move after publication");
            }
            Ok(())
        })
        .unwrap();
    }));
    assert_eq!(
        interrupted.unwrap_err().downcast_ref::<&str>(),
        Some(&"interrupted move after publication")
    );
    let (mut offers, errors) = scan(recovery);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(offers.len(), 1);
    let offer = offers.pop().unwrap();
    assert!(offer.moving);
    offer
}

fn retained_location(recovery: &Path, offer: &Offer) -> Location {
    let ticket = Ticket::open(receipts(recovery).join(&offer.id))
        .unwrap()
        .unwrap();
    let paths = ticket.receipt.paths().unwrap();
    let Some(HeldSource::Verified(record)) = &ticket.holding else {
        panic!("move has no checked source staging record");
    };
    staging_at(&paths.source, &record.stage_name)
        .unwrap()
        .join("source")
}

#[test]
fn published_move_requires_the_original_source_and_the_sealed_destination() {
    for tree in [false, true] {
        for replaced in [None, Some("source"), Some("target")] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let recovery = root.join("recovery");
            let source: Location = root.join("source").into();
            let target: Location = root.join("target").into();
            let payload = |location: &Location| {
                if tree {
                    location.path().join("child")
                } else {
                    location.path().to_owned()
                }
            };
            if tree {
                fs::create_dir(source.path()).unwrap();
            }
            let bytes = b"verified payload";
            fs::write(payload(&source), bytes).unwrap();
            let accessed = UNIX_EPOCH - Duration::new(86_400, 123_456_789);
            let set_accessed = |path: &Path, time| {
                fs::File::open(path)
                    .unwrap()
                    .set_times(fs::FileTimes::new().set_accessed(time))
                    .unwrap();
            };
            set_accessed(&payload(&source), accessed);
            if tree {
                set_accessed(source.path(), accessed + Duration::from_secs(7));
            }
            let item = TransferItem::new(source.clone(), source.clone(), target.clone());
            let offer = interrupt_move(&item, &recovery, Phase::Copied);

            if let Some(name) = replaced {
                let location = if name == "source" { &source } else { &target };
                fs::rename(location.path(), root.join("original")).unwrap();
                if tree {
                    fs::create_dir(location.path()).unwrap();
                }
                fs::write(payload(location), bytes).unwrap();
            }
            let (mut progress, mut view) = Progress::tracked_transfer();
            let result = resume(&recovery, &offer.id, &mut progress);
            assert_eq!(view.snapshot().total_written, 0);
            if replaced.is_none() {
                assert!(result.is_ok(), "{result:?}");
                assert_eq!(
                    fs::metadata(payload(&target)).unwrap().accessed().unwrap(),
                    accessed
                );
                if tree {
                    assert_eq!(
                        fs::metadata(target.path()).unwrap().accessed().unwrap(),
                        accessed + Duration::from_secs(7)
                    );
                }
                assert!(!source.path().exists());
                assert!(scan(&recovery).0.is_empty());
            } else {
                assert!(result.is_err(), "replacement of {replaced:?} was accepted");
                assert_eq!(fs::read(payload(&source)).unwrap(), bytes);
                assert_eq!(scan(&recovery).0.len(), 1);
            }
            assert_eq!(fs::read(payload(&target)).unwrap(), bytes);
        }
    }
}

#[test]
fn older_folder_moves_warn_and_a_damaged_access_time_section_preserves_the_source() {
    for legacy in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let recovery = root.join("recovery");
        let source: Location = root.join("source").into();
        let target: Location = root.join("target").into();
        fs::create_dir(source.path()).unwrap();
        fs::write(source.path().join("child"), b"source bytes").unwrap();
        let item = TransferItem::new(source.clone(), source.clone(), target.clone());
        let offer = interrupt_move(&item, &recovery, Phase::Copied);
        let path = receipts(&recovery).join(&offer.id);
        let mut bytes = fs::read(&path).unwrap();
        if legacy {
            let mut lines = bytes.split(|&byte| byte == b'\n');
            let mut header: serde_json::Value =
                serde_json::from_slice(lines.next().unwrap()).unwrap();
            header[0] = serde_json::Value::from(2);
            assert!(header[1]
                .as_object_mut()
                .unwrap()
                .remove("folder_access_times")
                .is_some());
            lines.next().expect("access time section");
            let mut previous = serde_json::to_vec(&header).unwrap();
            for line in lines {
                previous.push(b'\n');
                previous.extend_from_slice(line);
            }
            bytes = previous;
        } else {
            let section = bytes.iter().position(|&byte| byte == b'\n').unwrap() + 1;
            assert_eq!(bytes[section], b'[');
            bytes[section] = b'{';
        }
        fs::write(&path, bytes).unwrap();
        let mut progress = Progress::default();
        let result = resume(&recovery, &offer.id, &mut progress);
        if legacy {
            assert!(result.unwrap().contains(text!(
                "Даты или права доступа сохранены частично. Подробности указаны в строках журнала."
            )));
            assert!(progress.results.items.iter().all(|row| {
                row.outcome == Outcome::CompletedWithWarning
                    && row
                        .metadata_warning
                        .as_deref()
                        .is_some_and(|warning| warning.contains(text!("время доступа")))
            }));
            assert!(!source.path().exists());
            assert!(!path.exists());
        } else {
            assert!(result
                .unwrap_err()
                .contains(text!("Некорректная запись времени доступа для переноса")));
            assert_eq!(
                fs::read(source.path().join("child")).unwrap(),
                b"source bytes"
            );
            assert!(path.exists());
        }
        assert_eq!(
            fs::read(target.path().join("child")).unwrap(),
            b"source bytes"
        );
    }
}

#[test]
fn move_recovery_preserves_held_source_and_recognizes_acknowledged_removal() {
    for phase in [Phase::Quarantined, Phase::Removed] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let recovery = root.join("recovery");
        let source: Location = root.join("source").into();
        let target: Location = root.join("target").into();
        let bytes = b"original data";
        fs::write(source.path(), bytes).unwrap();
        let item = TransferItem::new(source.clone(), source.clone(), target.clone());
        let offer = interrupt_move(&item, &recovery, phase);
        let held = retained_location(&recovery, &offer);
        assert!(!source.path().exists());
        fs::write(source.path(), b"new source at the old path").unwrap();

        let result = resume(&recovery, &offer.id, &mut Progress::default());
        assert_eq!(
            fs::read(source.path()).unwrap(),
            b"new source at the old path"
        );
        assert_eq!(fs::read(target.path()).unwrap(), bytes);
        if phase == Phase::Quarantined {
            assert!(result.unwrap_err().contains(&held.display()));
            assert_eq!(fs::read(held.path()).unwrap(), bytes);
            assert_eq!(scan(&recovery).0.len(), 1);
        } else {
            assert!(result.is_ok(), "{result:?}");
            assert!(!held.path().exists());
            assert!(!held.parent().unwrap().path().exists());
            assert!(scan(&recovery).0.is_empty());
        }
    }
}

#[test]
fn move_recovery_finishes_verified_files_and_trees_around_source_rename() {
    for (tree, phase, restored) in [
        (false, Phase::QuarantinePlanned, false),
        (false, Phase::Quarantined, false),
        (true, Phase::Quarantined, false),
        (true, Phase::Quarantined, true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let recovery = root.join("recovery");
        let source: Location = root.join("tree").into();
        let target: Location = root.join("target").into();
        let bytes = b"complete and verified";
        if tree {
            setup_folder(source.path(), bytes);
            std::os::unix::fs::symlink("a-finished", source.join("link").path()).unwrap();
        } else {
            fs::write(source.path(), bytes).unwrap();
        }
        let item = TransferItem::new(source.clone(), source.clone(), target.clone());
        let offer = interrupt_move(&item, &recovery, phase);
        let held = retained_location(&recovery, &offer);
        assert_eq!(source.path().exists(), phase == Phase::QuarantinePlanned);
        if restored {
            fs::rename(held.path(), source.path()).unwrap();
        }
        let before = fs::metadata(target.path()).unwrap();
        let (mut progress, mut view) = Progress::tracked_transfer();
        resume(&recovery, &offer.id, &mut progress).unwrap();
        assert_eq!(view.snapshot().total_written, 0);
        assert_eq!(before.ino(), fs::metadata(target.path()).unwrap().ino());
        if tree {
            assert_eq!(fs::read(target.join("a-finished").path()).unwrap(), HEAD);
            assert_eq!(
                fs::read(target.join("z-child/large").path()).unwrap(),
                bytes
            );
            assert_eq!(
                fs::read_link(target.join("link").path()).unwrap(),
                Path::new("a-finished")
            );
            assert!(target.join("z-child/empty").path().is_dir());
            assert_eq!(progress.results.items.len(), 6);
        } else {
            assert_eq!(fs::read(target.path()).unwrap(), bytes);
        }
        assert!(progress
            .results
            .items
            .iter()
            .all(|row| row.outcome == Outcome::Completed));
        assert!(!source.path().exists());
        assert!(!held.parent().unwrap().path().exists());
        assert!(scan(&recovery).0.is_empty());
    }
}

#[test]
fn move_recovery_keeps_an_incomplete_or_unowned_held_tree() {
    for problem in ["missing child", "unexpected sibling", "legacy receipt"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let recovery = root.join("recovery");
        let source: Location = root.join("tree").into();
        let target: Location = root.join("target").into();
        let bytes = b"original file bytes";
        setup_folder(source.path(), bytes);
        let item = TransferItem::new(source.clone(), source.clone(), target.clone());
        let offer = interrupt_move(&item, &recovery, Phase::Quarantined);
        let held = retained_location(&recovery, &offer);
        let sibling = held.parent().unwrap().join("another file");
        if problem == "missing child" {
            fs::remove_file(held.join("a-finished").path()).unwrap();
        } else if problem == "unexpected sibling" {
            fs::write(sibling.path(), b"keep this file").unwrap();
        } else {
            let receipt = receipts(&recovery).join(&offer.id);
            let journal = fs::read_to_string(&receipt).unwrap();
            let (prefix, _) = journal.rsplit_once('\n').unwrap();
            let legacy =
                serde_json::to_string(&Event::Removing(HeldSource::Legacy((&held).into())))
                    .unwrap();
            fs::write(&receipt, format!("{prefix}\n{legacy}")).unwrap();
        }
        let (mut progress, mut view) = Progress::tracked_transfer();
        let error = resume(&recovery, &offer.id, &mut progress).unwrap_err();
        assert!(error.contains(&held.display()), "{problem}: {error}");
        assert_eq!(view.snapshot().total_written, 0);
        assert_eq!(fs::read(held.join("z-child/large").path()).unwrap(), bytes);
        assert_eq!(fs::read(target.join("a-finished").path()).unwrap(), HEAD);
        assert_eq!(
            fs::read(target.join("z-child/large").path()).unwrap(),
            bytes
        );
        assert!(!source.path().exists());
        if problem == "unexpected sibling" {
            assert_eq!(fs::read(sibling.path()).unwrap(), b"keep this file");
        }
        assert_eq!(scan(&recovery).0.len(), 1);
    }
}

fn check_sftp_files(root: &Path, remote_root: &Location, kind: TransferKind) {
    use std::net::{Shutdown, TcpStream};
    use std::os::fd::{AsRawFd, BorrowedFd};

    for upload in [true, false] {
        let name = reconnect_name("file", upload, kind);
        let remote = remote_root.join(&name);
        let local: Location = root.join(&name).into();
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
        let result = first_transfer(&source, output, &recovery, kind, move || {
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

fn reconnect_name(shape: &str, upload: bool, kind: TransferKind) -> String {
    let action = if kind == TransferKind::Move {
        "move"
    } else {
        "copy"
    };
    let direction = if upload { "upload" } else { "download" };
    format!("resume-{action}-{shape}-{direction}")
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
    if offers[0].moving {
        assert!(!source.exists());
    }
    for root in [source, target]
        .into_iter()
        .filter(|&root| root != source || !offers[0].moving)
    {
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

fn check_sftp_folders(root: &Path, remote_root: &Location, kind: TransferKind) {
    use std::net::{Shutdown, TcpStream};
    use std::os::fd::{AsRawFd, BorrowedFd};
    let bytes: Vec<_> = (0..768 * 1024).map(|i| (i % 239) as u8).collect();
    for upload in [true, false] {
        let name = reconnect_name("folder", upload, kind);
        let remote = remote_root.join(&name);
        let local: Location = root.join(&name).into();
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
        let error = first_transfer(&source, output, &recovery, kind, move || {
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
