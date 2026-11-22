use super::*;
use crate::conflict::{Answer, Decision};
use crate::file_error::{
    Failure, LocalPathAction, MutationError, PublicationError, RecoveryRecordSection, ScanError,
};
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
    assert_eq!(result.kind, Completion::Copied(offset));
    assert_eq!(result.moving, offers[0].moving);
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

fn retained_check<'a>(error: &'a TransferError, stage: &Path) -> &'a TransferError {
    let TransferError::Failed(Failure::Publication(publication)) = error else {
        panic!("Expected a retained copy, got {error:?}");
    };
    let PublicationError::Retained { staging, cause, .. } = publication.as_ref() else {
        panic!("Expected a retained copy, got {publication:?}");
    };
    assert_eq!(staging.path(), stage);
    cause
}

fn assert_resume_check(
    error: &TransferError,
    role: ResumeRole,
    path: &Path,
    problem: ResumeCheckProblem,
) {
    let TransferError::Failed(Failure::ResumeCheck(check)) = error else {
        panic!("Expected a recovery check at {path:?}, got {error:?}");
    };
    assert_eq!(check.role, role);
    assert_eq!(check.location.path(), path);
    assert_eq!(check.problem, problem);
}

#[test]
fn unreadable_recovery_records_report_their_path_and_cause() {
    let temp = tempfile::tempdir().unwrap();
    let recovery = temp.path();
    let directory = receipts(recovery);
    fs::create_dir(&directory).unwrap();
    let malformed = directory.join("copy-malformed.json");
    let future = directory.join("copy-future.json");
    let symlink = directory.join("copy-link.json");
    let sentinel = recovery.join("sentinel");
    fs::write(&malformed, b"[1,{").unwrap();
    fs::write(&future, br#"[99,{"future":true}]"#).unwrap();
    fs::write(&sentinel, b"keep").unwrap();
    std::os::unix::fs::symlink(&sentinel, &symlink).unwrap();

    let (offers, errors) = scan(recovery);
    assert!(offers.is_empty());
    assert_eq!(errors.len(), 3);
    for error in errors {
        match error.problem {
            RecoveryRecordProblem::Json {
                section: RecoveryRecordSection::Header,
                source,
            } => {
                assert_eq!(error.path, malformed);
                assert_eq!(source.line(), 1);
                assert!(source.column() > 0);
            }
            RecoveryRecordProblem::UnsupportedVersion(99) => assert_eq!(error.path, future),
            RecoveryRecordProblem::Read(source) => {
                assert_eq!(error.path, symlink);
                assert_eq!(source.raw_os_error(), Some(libc::ELOOP));
            }
            other => panic!("Unexpected problem for {:?}: {other:?}", error.path),
        }
    }

    let (mut progress, mut view) = Progress::tracked_transfer();
    let error = resume(recovery, "copy-future.json", &mut progress).unwrap_err();
    assert!(matches!(
        error,
        TransferError::Failed(Failure::RecoveryRecord(error))
            if error.path == future && matches!(error.problem, RecoveryRecordProblem::UnsupportedVersion(99))
    ));
    let [row] = progress.results.items.as_slice() else {
        panic!("Expected one history row for the failing record");
    };
    assert_eq!(row.outcome, Outcome::Failed);
    assert_eq!(row.source, future.display().to_string());
    assert_eq!(view.snapshot().total_written, 0);
    assert_eq!(fs::read(&malformed).unwrap(), b"[1,{");
    assert_eq!(fs::read(&future).unwrap(), br#"[99,{"future":true}]"#);
    assert_eq!(fs::read_link(symlink).unwrap(), sentinel);
    assert_eq!(fs::read(sentinel).unwrap(), b"keep");
}

#[test]
fn retained_copy_restarts_from_checked_offset_and_never_retries_changed_data() {
    for change in [
        None,
        Some("source"),
        Some("target"),
        Some("payload"),
        Some("size"),
        Some("backup"),
        Some("parked"),
        Some("published"),
        Some("published-no-backup"),
        Some("published-no-marker"),
        Some("published-other-stage"),
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
        if change == "parked" || change.starts_with("published") {
            let stage = partial.parent().unwrap();
            fs::rename(&target, stage.join("previous")).unwrap();
            if change == "parked" {
                continued(&recovery, &partial, &source, &target, &bytes);
            } else {
                fs::write(&partial, &bytes).unwrap();
                fs::rename(&partial, &target).unwrap();
                let (offers, errors) = scan(&recovery);
                assert!(errors.is_empty());
                if change == "published-no-backup" || change == "published-no-marker" {
                    fs::remove_file(stage.join("previous")).unwrap();
                    if change == "published-no-marker" {
                        fs::remove_file(stage.join(&offers[0].id)).unwrap();
                    }
                }
                let original_stage = root.join("original-stage");
                if change == "published-other-stage" {
                    fs::rename(stage, &original_stage).unwrap();
                    fs::create_dir(stage).unwrap();
                }
                let published = fs::metadata(&target).unwrap();
                let (mut progress, mut view) = Progress::tracked_transfer();
                let result = resume(&recovery, &offers[0].id, &mut progress);
                if change == "published-other-stage" {
                    let error = result.unwrap_err();
                    assert!(error.is_completed_cleanup_warning());
                    let TransferError::Failed(Failure::StagingCleanup(cleanup)) = error else {
                        panic!("expected a completed copy with a staging warning");
                    };
                    assert_eq!(cleanup.staging.path(), stage);
                    assert_eq!(cleanup.target.path(), target);
                    assert!(matches!(
                        cleanup.cleanup,
                        TransferError::Failed(Failure::Staging(error))
                            if matches!(error.as_ref(), StagingError::IdentityChanged(path)
                                if path.path() == stage)
                    ));
                    assert_eq!(
                        progress.results.items[0].outcome,
                        Outcome::CompletedWithWarning
                    );
                    assert!(stage.is_dir());
                    assert_eq!(
                        fs::read(original_stage.join("previous")).unwrap(),
                        b"previous destination"
                    );
                    assert_eq!(scan(&recovery).0.len(), 1);
                } else {
                    assert_eq!(result.unwrap().kind, Completion::AlreadyPresent);
                    assert!(!stage.exists());
                    assert!(scan(&recovery).0.is_empty());
                }
                assert!(!progress.partial_copy_retained);
                assert_eq!(view.snapshot().total_written, 0);
                assert_eq!(fs::metadata(&target).unwrap().ino(), published.ino());
                assert_eq!(fs::read(&target).unwrap(), bytes);
            }
            continue;
        }
        let backup = partial.parent().unwrap().join("previous");
        let (role, changed, problem) = match change {
            "source" => (ResumeRole::Source, &source, ResumeCheckProblem::Changed),
            "target" => (
                ResumeRole::Destination,
                &target,
                ResumeCheckProblem::Changed,
            ),
            "backup" => {
                fs::rename(&target, &backup).unwrap();
                (
                    ResumeRole::ParkedDestination,
                    &backup,
                    ResumeCheckProblem::ContentsMismatch,
                )
            }
            "size" => (
                ResumeRole::Staged,
                &partial,
                ResumeCheckProblem::SizeMismatch {
                    expected: bytes.len() as u64,
                    actual: bytes.len() as u64 + 1,
                },
            ),
            _ => (
                ResumeRole::Staged,
                &partial,
                ResumeCheckProblem::ContentsMismatch,
            ),
        };
        let times = changed.metadata().unwrap();
        let mut file = fs::OpenOptions::new().write(true).open(changed).unwrap();
        if change == "size" {
            file.set_len(bytes.len() as u64 + 1).unwrap();
        } else {
            file.seek(SeekFrom::Start(2)).unwrap();
            file.write_all(b"changed").unwrap();
        }
        file.set_times(fs::FileTimes::new().set_modified(times.modified().unwrap()))
            .unwrap();
        let previous = if change == "backup" { &backup } else { &target };
        let snapshot = [&source, previous, &partial].map(|path| fs::read(path).unwrap());
        let (offers, errors) = scan(&recovery);
        assert!(errors.is_empty());
        assert_eq!(offers.len(), 1);
        let (mut progress, mut view) = Progress::tracked_transfer();
        let error = resume(&recovery, &offers[0].id, &mut progress).unwrap_err();
        assert_resume_check(
            retained_check(&error, partial.parent().unwrap()),
            role,
            changed,
            problem,
        );
        assert_eq!(
            progress.results.items[0].outcome,
            Outcome::PartialCopyRetained
        );
        assert_eq!(view.snapshot().total_written, 0, "{change}");
        assert_eq!(
            snapshot,
            [&source, previous, &partial].map(|path| fs::read(path).unwrap())
        );
        assert_eq!(target.exists(), change != "backup");
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
    let paths = ticket.paths().unwrap();
    let Some(HeldSource::Verified(record)) = &ticket.holding else {
        panic!("move has no checked source staging record");
    };
    staging_at(&paths.source, &record.stage_name)
        .unwrap()
        .join("source")
}

#[test]
fn a_replaced_receipt_does_not_make_a_confirmed_move_uncertain() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let recovery = root.join("recovery");
    let original: Location = root.join("source").into();
    let target: Location = root.join("target").into();
    fs::write(original.path(), b"move this data").unwrap();
    let item = TransferItem::new(original.clone(), original.clone(), target.clone());
    let mut progress = Progress::default();
    progress.resume_directory = Some(recovery.clone());
    let saved = root.join("original-receipt");
    let mut replaced = None;
    let mut published = false;
    let mut removed = false;
    let error = cross_move::run(
        &item,
        None,
        &mut progress,
        || published = true,
        || removed = true,
        &mut |_, phase| {
            if phase == Phase::Removed {
                let path = fs::read_dir(receipts(&recovery))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                fs::rename(&path, &saved).unwrap();
                fs::write(&path, b"preserve this record").unwrap();
                replaced = Some(path);
            }
            Ok(())
        },
    )
    .unwrap_err();
    let replaced = replaced.unwrap();
    let TransferError::Failed(Failure::Publication(error)) = error else {
        panic!("Expected a recovery finalization warning after a confirmed move");
    };
    assert!(matches!(
        *error,
        PublicationError::RecoveryFinalization { target: location, moving: true, cause }
            if location == target && cause.path == replaced && matches!(cause.problem, RecoveryRecordProblem::IdentityChanged)
    ));
    assert!(published && removed && !progress.partial_copy_retained);
    assert!(!original.path().exists());
    assert_eq!(fs::read(target.path()).unwrap(), b"move this data");
    assert_eq!(fs::read(&replaced).unwrap(), b"preserve this record");
    assert!(Ticket::open(saved).unwrap().unwrap().removed);
}

#[test]
fn stopping_a_resumed_copy_reports_receipt_failure_after_draft_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let recovery = root.join("recovery");
    let output = root.join("destination");
    let original = root.join("source");
    let bytes = vec![9; 1024 * 1024];
    fs::create_dir(&output).unwrap();
    fs::write(&original, &bytes).unwrap();
    let observed = output.clone();
    let interrupted = std::panic::catch_unwind(|| {
        first_copy(
            &original.clone().into(),
            &output.clone().into(),
            &recovery,
            move || {
                if staged_payload(&observed)
                    .and_then(|file| file.metadata().ok())
                    .is_some_and(|meta| meta.len() >= 128 * 1024)
                {
                    std::panic::panic_any("interrupted copy");
                }
                false
            },
        )
    });
    assert_eq!(
        interrupted.unwrap_err().downcast_ref::<&str>(),
        Some(&"interrupted copy")
    );
    let (offers, errors) = scan(&recovery);
    assert!(errors.is_empty() && offers.len() == 1);
    let record = receipts(&recovery).join(&offers[0].id);
    let valid = fs::read(&record).unwrap();
    let draft = staged_payload(&output).unwrap();
    let offset = draft.metadata().unwrap().len();
    let saved = root.join("original-receipt");
    let (original_record, saved_record) = (record.clone(), saved.clone());
    let (mut progress, _) = Progress::tracked_transfer();
    progress.cancellation().set_checkpoint(move || {
        if fs::metadata(&draft).is_ok_and(|meta| meta.len() > offset) {
            fs::rename(&original_record, &saved_record).unwrap();
            fs::write(&original_record, b"preserve the replacement").unwrap();
            return true;
        }
        false
    });
    let error = resume(&recovery, &offers[0].id, &mut progress).unwrap_err();
    assert!(matches!(
        error,
        TransferError::Failed(Failure::Staging(error))
            if matches!(error.as_ref(), StagingError::ResumeReceiptCleanup {
                cause: TransferError::Cancelled, source,
            } if source.path == record && matches!(source.problem, RecoveryRecordProblem::IdentityChanged))
    ));
    assert!(progress.results.cancelled);
    assert_eq!(progress.results.items[0].outcome, Outcome::Failed);
    assert!(!progress.partial_copy_retained && staged_payload(&output).is_none());
    assert!(!output.join("source").exists());
    assert_eq!(fs::read(original).unwrap(), bytes);
    assert_eq!(fs::read(saved).unwrap(), valid);
    assert_eq!(fs::read(record).unwrap(), b"preserve the replacement");
}

#[test]
fn stopping_after_parking_refuses_to_restore_a_different_file() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let recovery = root.join("recovery");
    let source: Location = root.join("transfer.bin").into();
    let output: Location = root.join("destination").into();
    let target = output.join("transfer.bin");
    let original = root.join("old-version");
    let old_bytes = b"previous version";
    fs::create_dir(output.path()).unwrap();
    fs::write(source.path(), b"replacement bytes").unwrap();
    fs::write(target.path(), old_bytes).unwrap();
    let mut progress = Progress::tracked_transfer().0;
    progress.resume_directory = Some(recovery.clone());
    let observed = output.clone();
    let saved = original.clone();
    progress.cancellation().set_checkpoint(move || {
        let Some(payload) = staged_payload(observed.path()) else {
            return false;
        };
        let backup = payload.with_file_name("previous");
        if !backup.exists() {
            return false;
        }
        fs::rename(&backup, &saved).unwrap();
        fs::write(&backup, old_bytes).unwrap();
        true
    });
    let failure = transfer_with_conflicts(
        std::slice::from_ref(&source),
        &output,
        TransferKind::Copy,
        &mut progress,
        |_| {
            Ok(Answer {
                decision: Decision::Replace,
                apply_to_group: false,
            })
        },
        |_, _| panic!("a copy must preserve the source"),
    )
    .unwrap_err();
    assert!(failure.summary.cancelled && progress.results.cancelled);
    assert_eq!(failure.summary.completed, 0);
    assert_eq!(
        progress.results.items[0].outcome,
        Outcome::PartialCopyRetained
    );
    let draft = staged_payload(output.path()).unwrap();
    let stage = draft.parent().unwrap();
    let backup = draft.with_file_name("previous");
    let TransferError::Failed(Failure::Publication(reason)) = failure.error else {
        panic!("expected a failed restoration with its retained staging");
    };
    let PublicationError::Retained {
        staging,
        cause: TransferError::Failed(Failure::Publication(reason)),
        ..
    } = *reason
    else {
        panic!("expected a retained recovery record");
    };
    assert_eq!(staging.path(), stage);
    let PublicationError::Rollback {
        target: attempted_target,
        backup: attempted_backup,
        cause: TransferError::Cancelled,
        restore,
    } = *reason
    else {
        panic!("expected a typed restoration error after Stop");
    };
    assert_eq!(attempted_target, target);
    assert_eq!(attempted_backup.path(), backup);
    assert_resume_check(
        &restore,
        ResumeRole::ParkedDestination,
        &backup,
        ResumeCheckProblem::Changed,
    );
    assert!(!target.path().exists());
    assert_eq!(fs::read(source.path()).unwrap(), b"replacement bytes");
    assert_eq!(fs::read(original).unwrap(), old_bytes);
    assert_eq!(fs::read(backup).unwrap(), old_bytes);
    assert_eq!(fs::read(draft).unwrap(), b"replacement bytes");
    assert_eq!(scan(&recovery).0.len(), 1);
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
            assert!(result.unwrap().metadata_warning);
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
            let TransferError::Failed(Failure::Mutation(error)) = result.unwrap_err() else {
                panic!("Expected an incomplete move with a recovery record error");
            };
            let MutationError::MoveIncomplete {
                target: saved_target,
                cause,
            } = *error
            else {
                panic!("Expected the published target in the incomplete move error");
            };
            assert_eq!(saved_target, target);
            assert!(matches!(
                cause,
                TransferError::Failed(Failure::RecoveryRecord(error))
                    if error.path == path && matches!(error.problem, RecoveryRecordProblem::InvalidAccessTimes)
            ));
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
            assert!(crate::i18n::file_error(&result.unwrap_err()).contains(&held.display()));
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
fn draft_receipt_failure_removes_only_the_empty_preparation() {
    for folder in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let parent = root.join("destination");
        let source: Location = root.join("source").into();
        let target: Location = parent.join("data").into();
        let recovery = root.join("recovery");
        fs::create_dir(&parent).unwrap();
        if folder {
            fs::create_dir(source.path()).unwrap();
        } else {
            fs::write(source.path(), b"original").unwrap();
            fs::write(target.path(), b"older destination").unwrap();
        }
        fs::write(&recovery, b"keep the existing recovery path").unwrap();
        let item = TransferItem::new(source.clone(), source.clone(), target.clone());
        let mut progress = Progress::default();
        let expected = entry_metadata(&target).unwrap();
        let published = || panic!("publication must not start without a receipt");
        let result = if folder {
            copy_folder(&item, &recovery, &mut progress, published)
        } else {
            copy(
                &item,
                expected.as_ref(),
                &recovery,
                &mut progress,
                published,
            )
        };
        let TransferError::Failed(Failure::Staging(error)) = result.unwrap_err() else {
            panic!("expected a receipt creation error");
        };
        assert!(matches!(
            *error,
            StagingError::ResumeReceipt { staging, source }
                if staging.parent().unwrap().path() == parent && source.path == receipts(&recovery)
        ));
        assert!(staged_payload(&parent).is_none());
        assert!(!progress.partial_copy_retained);
        assert_eq!(
            fs::read(&recovery).unwrap(),
            b"keep the existing recovery path"
        );
        if folder {
            assert!(source.path().is_dir() && !target.path().exists());
        } else {
            assert_eq!(fs::read(source.path()).unwrap(), b"original");
            assert_eq!(fs::read(target.path()).unwrap(), b"older destination");
        }
    }
}

#[test]
fn stopped_preparation_preserves_a_replaced_file_or_nonempty_folder() {
    for folder in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let parent = root.join("destination");
        let source: Location = root.join("source").into();
        let target: Location = parent.join("data").into();
        let recovery = root.join("recovery");
        fs::create_dir(&parent).unwrap();
        if folder {
            fs::create_dir(source.path()).unwrap();
        } else {
            fs::write(source.path(), b"original").unwrap();
        }
        let item = TransferItem::new(source.clone(), source.clone(), target.clone());
        let mut progress = Progress::default();
        let saved = root.join("original-empty-payload");
        let (watch, saved_payload) = (parent.clone(), saved.clone());
        progress.cancellation().set_checkpoint(move || {
            let Some(payload) = staged_payload(&watch).filter(|path| path.exists()) else {
                return false;
            };
            if folder {
                fs::write(payload.join("foreign"), b"preserve me").unwrap();
            } else {
                // Keep the old inode live so the replacement has a different identity.
                fs::rename(&payload, &saved_payload).unwrap();
                fs::write(&payload, b"").unwrap();
            }
            true
        });
        let published = || panic!("publication must not start after Stop");
        let result = if folder {
            copy_folder(&item, &recovery, &mut progress, published)
        } else {
            copy(&item, None, &recovery, &mut progress, published)
        };
        let TransferError::Failed(Failure::StagingCleanup(error)) = result.unwrap_err() else {
            panic!("expected both cancellation and cleanup failure");
        };
        let payload = staged_payload(&parent).unwrap();
        assert!(matches!(error.operation, Some(TransferError::Cancelled)));
        assert!(progress.results.cancelled);
        assert_eq!(error.staging.path(), payload.parent().unwrap());
        assert!(!progress.partial_copy_retained && !target.path().exists());
        assert!(scan(&recovery).0.is_empty());
        if folder {
            assert!(matches!(
                error.cleanup,
                TransferError::Failed(Failure::Access(access))
                    if access.location.path() == payload
                        && matches!(access.action, AccessAction::RemoveStagingDirectory)
                        && matches!(&access.cause, crate::file_error::AccessCause::Local(cause)
                            if cause.raw_os_error() == Some(libc::ENOTEMPTY))
            ));
            assert_eq!(fs::read(payload.join("foreign")).unwrap(), b"preserve me");
        } else {
            assert_resume_check(
                &error.cleanup,
                ResumeRole::Staged,
                &payload,
                ResumeCheckProblem::Changed,
            );
            assert_eq!(fs::metadata(&payload).unwrap().len(), 0);
            assert_ne!(
                fs::metadata(&payload).unwrap().ino(),
                fs::metadata(&saved).unwrap().ino()
            );
            assert_eq!(fs::read(source.path()).unwrap(), b"original");
        }
    }
}

#[test]
fn aborted_move_preparation_cleans_only_known_staging() {
    for stop in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let recovery = root.join("recovery");
        let source: Location = root.join("left/data").into();
        let target: Location = root.join("right/data").into();
        fs::create_dir(source.parent().unwrap().path()).unwrap();
        fs::create_dir(target.parent().unwrap().path()).unwrap();
        fs::write(source.path(), b"original data").unwrap();
        let original_inode = fs::metadata(source.path()).unwrap().ino();
        let item = TransferItem::new(source.clone(), source.clone(), target.clone());
        let mut progress = Progress::default();
        progress.resume_directory = Some(recovery.clone());
        let cancel = progress.cancellation();
        let saved = root.join("saved-receipt");
        let mut replaced = None;
        let mut stage = None;
        let mut published = false;
        let error = cross_move::run(
            &item,
            None,
            &mut progress,
            || published = true,
            || panic!("source removal must not run"),
            &mut |path, phase| {
                match phase {
                    Phase::Copied if !stop => {
                        let receipt = fs::read_dir(receipts(&recovery))
                            .unwrap()
                            .next()
                            .unwrap()
                            .unwrap()
                            .path();
                        fs::rename(&receipt, &saved).unwrap();
                        fs::write(&receipt, b"keep the unrelated receipt").unwrap();
                        replaced = Some(receipt);
                    }
                    Phase::QuarantinePlanned if stop => cancel.request(),
                    Phase::QuarantineAborted => {
                        let directory = path.parent().unwrap();
                        if stop {
                            fs::write(directory.join("independent").path(), b"keep this").unwrap();
                        }
                        stage = Some(directory);
                    }
                    Phase::Quarantined => panic!("the source must never be moved into staging"),
                    _ => {}
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(published && !progress.partial_copy_retained);
        assert_eq!(progress.results.cancelled, stop);
        assert_eq!(fs::read(source.path()).unwrap(), b"original data");
        assert_eq!(fs::metadata(source.path()).unwrap().ino(), original_inode);
        assert_eq!(fs::read(target.path()).unwrap(), b"original data");
        let stage = stage.unwrap();
        assert!(!stage.join("source").path().exists());
        let TransferError::Failed(Failure::Mutation(error)) = error else {
            panic!("expected the incomplete move");
        };
        let MutationError::MoveIncomplete {
            target: copy,
            cause,
        } = *error
        else {
            panic!("expected the incomplete move");
        };
        assert_eq!(copy, target);
        let operation = if stop {
            let TransferError::Failed(Failure::StagingCleanup(error)) = cause else {
                panic!("expected both cancellation and cleanup failure");
            };
            assert_eq!(error.staging, stage);
            assert!(!error.completed_move);
            let independent = stage.join("independent");
            assert_resume_check(
                &error.cleanup,
                ResumeRole::Staged,
                independent.path(),
                ResumeCheckProblem::Unexpected,
            );
            assert_eq!(fs::read(independent.path()).unwrap(), b"keep this");
            fs::remove_file(independent.path()).unwrap();
            error.operation.unwrap()
        } else {
            assert!(!stage.path().exists());
            cause
        };
        let TransferError::Failed(Failure::CrossMove(error)) = operation else {
            panic!("expected a source preparation error");
        };
        let CrossMoveError::Prepare { original, cause } = *error else {
            panic!("expected the original path and the preparation cause");
        };
        assert_eq!(original, source);
        if stop {
            assert!(matches!(cause, TransferError::Cancelled));
        } else {
            let receipt = replaced.unwrap();
            assert!(matches!(
                cause, TransferError::Failed(Failure::RecoveryRecord(error))
                    if error.path == receipt && matches!(error.problem, RecoveryRecordProblem::IdentityChanged)
            ));
            assert_eq!(fs::read(&receipt).unwrap(), b"keep the unrelated receipt");
            fs::rename(&saved, receipt).unwrap();
        }
        let (offers, errors) = scan(&recovery);
        assert!(errors.is_empty() && offers.len() == 1);
        let existing_copy = fs::metadata(target.path()).unwrap().ino();
        let (mut progress, mut view) = Progress::tracked_transfer();
        let result = resume(&recovery, &offers[0].id, &mut progress).unwrap();
        assert!(result.moving && result.kind == Completion::AlreadyPresent);
        assert_eq!(view.snapshot().total_written, 0);
        assert_eq!(fs::metadata(target.path()).unwrap().ino(), existing_copy);
        assert!(!source.path().exists() && !stage.path().exists());
        assert!(scan(&recovery).0.is_empty());
    }
}

#[test]
fn restored_move_preserves_the_primary_error_and_an_unrelated_staging_entry() {
    for stop in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let recovery = root.join("recovery");
        let source: Location = root.join("left/data").into();
        let target: Location = root.join("right/data").into();
        let remaining = source.join("a");
        let removed = source.join("z");
        fs::create_dir_all(source.path()).unwrap();
        fs::create_dir_all(target.parent().unwrap().path()).unwrap();
        fs::write(remaining.path(), b"remaining").unwrap();
        fs::write(removed.path(), b"last entry").unwrap();

        let item = TransferItem::new(source.clone(), source.clone(), target.clone());
        let mut progress = Progress::default();
        progress.resume_directory = Some(recovery.clone());
        progress
            .results
            .plan([(source.clone(), Some(target.display()))]);
        progress.results.begin(&source);
        let cancellation = progress.cancellation();
        let mut published = false;
        let mut unrelated = None;
        let error = cross_move::run(
            &item,
            None,
            &mut progress,
            || published = true,
            || panic!("move must remain incomplete"),
            &mut |location, phase| {
                if phase == Phase::BeforeRemove && (stop || location.name() == "a") {
                    if stop {
                        cancellation.request();
                    } else {
                        return Err(MutationError::local(
                            location.path(),
                            LocalPathAction::RemoveFile,
                            std::io::Error::from_raw_os_error(libc::EIO),
                        )
                        .into());
                    }
                }
                if phase == Phase::Restored {
                    let entry = location.parent().unwrap().join("independent");
                    fs::write(entry.path(), b"preserve this staging entry").unwrap();
                    unrelated = Some(entry);
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(published);
        assert_eq!(progress.results.cancelled, stop);
        let last_entry = progress
            .results
            .items
            .iter()
            .find(|row| row.source == removed.display())
            .unwrap();
        assert_eq!(
            last_entry.outcome,
            if stop {
                Outcome::CopiedSourceRemaining
            } else {
                Outcome::Completed
            }
        );
        assert_eq!(fs::read(remaining.path()).unwrap(), b"remaining");
        assert_eq!(removed.path().exists(), stop);
        assert_eq!(fs::read(target.join("a").path()).unwrap(), b"remaining");
        assert_eq!(fs::read(target.join("z").path()).unwrap(), b"last entry");

        let unrelated = unrelated.unwrap();
        let stage = unrelated.parent().unwrap();
        assert_eq!(
            fs::read(unrelated.path()).unwrap(),
            b"preserve this staging entry"
        );
        let TransferError::Failed(Failure::Mutation(error)) = error else {
            panic!("expected the incomplete move");
        };
        let MutationError::MoveIncomplete {
            target: published,
            cause,
        } = *error
        else {
            panic!("expected the incomplete move");
        };
        assert_eq!(published, target);
        let TransferError::Failed(Failure::StagingCleanup(error)) = cause else {
            panic!("expected the cleanup error after restoration");
        };
        assert!(!error.completed_move);
        assert_eq!(error.staging, stage);
        assert_resume_check(
            &error.cleanup,
            ResumeRole::Staged,
            unrelated.path(),
            ResumeCheckProblem::Unexpected,
        );
        let Some(TransferError::Failed(Failure::CrossMove(restored))) = error.operation else {
            panic!("expected the confirmed restoration");
        };
        let CrossMoveError::Restored {
            original,
            cause: Some(primary),
        } = *restored
        else {
            panic!("expected the restored path and the primary error");
        };
        assert_eq!(original, source);
        if stop {
            assert!(matches!(primary, TransferError::Cancelled));
        } else {
            assert!(matches!(
                primary, TransferError::Failed(Failure::Mutation(error))
                    if matches!(error.as_ref(), MutationError::LocalIo { path, source, .. }
                        if path == stage.join("source").join("a").path()
                            && source.raw_os_error() == Some(libc::EIO))
            ));
        }

        let (offers, errors) = scan(&recovery);
        assert!(errors.is_empty() && offers.len() == 1);
        let id = &offers[0].id;
        let ticket = Ticket::open(receipts(&recovery).join(id)).unwrap().unwrap();
        assert!(!ticket.removed);
        let owner = stage.join(ticket.name());
        drop(ticket);
        let original_target = fs::metadata(target.path()).unwrap().ino();
        if stop {
            fs::remove_file(unrelated.path()).unwrap();
            fs::remove_file(owner.path()).unwrap();
            let (mut progress, mut view) = Progress::tracked_transfer();
            let result = resume(&recovery, id, &mut progress).unwrap();
            assert!(result.moving && result.kind == Completion::AlreadyPresent);
            assert_eq!(view.snapshot().total_written, 0);
            assert!(!source.path().exists() && !stage.path().exists());
            assert!(scan(&recovery).0.is_empty());
        } else {
            assert!(resume(&recovery, id, &mut Progress::default()).is_err());
            assert_eq!(fs::read(remaining.path()).unwrap(), b"remaining");
            assert_eq!(
                fs::read(unrelated.path()).unwrap(),
                b"preserve this staging entry"
            );
        }
        assert_eq!(fs::metadata(target.path()).unwrap().ino(), original_target);
    }
}

#[test]
fn completed_move_preserves_unexpected_staging_entries_and_resumes_only_cleanup() {
    for source_side in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let recovery = root.join("recovery");
        let source: Location = root.join("left/data").into();
        let target: Location = root.join("right/data").into();
        fs::create_dir_all(source.parent().unwrap().path()).unwrap();
        fs::create_dir_all(target.parent().unwrap().path()).unwrap();
        fs::write(source.path(), b"original data").unwrap();
        let item = TransferItem::new(source.clone(), source.clone(), target.clone());
        let mut progress = Progress::default();
        progress.resume_directory = Some(recovery.clone());
        let mut published = false;
        let mut removed = false;
        let mut unrelated = None;
        let error = cross_move::run(
            &item,
            None,
            &mut progress,
            || published = true,
            || removed = true,
            &mut |location, phase| {
                if phase == Phase::Removed {
                    let stage = if source_side {
                        location.parent().unwrap()
                    } else {
                        let parent = target.parent().unwrap();
                        let payload = staged_payload(parent.path()).unwrap();
                        payload.parent().unwrap().to_owned().into()
                    };
                    let entry = stage.join("independent");
                    fs::write(entry.path(), b"preserve this staging entry").unwrap();
                    unrelated = Some(entry);
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(published && removed && !progress.partial_copy_retained);
        assert!(!source.path().exists());
        assert_eq!(fs::read(target.path()).unwrap(), b"original data");
        let original_target = fs::metadata(target.path()).unwrap();
        let unrelated = unrelated.unwrap();
        assert!(error.is_completed_cleanup_warning());
        let TransferError::Failed(Failure::StagingCleanup(error)) = error else {
            panic!("expected a completed move with an exact cleanup warning");
        };
        assert!(error.completed_move && error.operation.is_none());
        assert_eq!(error.target, target);
        assert_eq!(Some(&error.staging), unrelated.parent().as_ref());
        assert_resume_check(
            &error.cleanup,
            ResumeRole::Staged,
            unrelated.path(),
            ResumeCheckProblem::Unexpected,
        );
        assert_eq!(
            fs::read(unrelated.path()).unwrap(),
            b"preserve this staging entry"
        );
        let (offers, errors) = scan(&recovery);
        assert!(errors.is_empty() && offers.len() == 1);
        let offer = &offers[0];
        assert!(
            Ticket::open(receipts(&recovery).join(&offer.id))
                .unwrap()
                .unwrap()
                .removed
        );
        fs::write(source.path(), b"new source at the old path").unwrap();

        let (mut progress, mut view) = Progress::tracked_transfer();
        let error = resume(&recovery, &offer.id, &mut progress).unwrap_err();
        assert!(error.is_completed_cleanup_warning());
        assert_eq!(
            progress.results.items[0].outcome,
            Outcome::CompletedWithWarning
        );
        assert_eq!(view.snapshot().total_written, 0);
        assert_eq!(
            fs::read(unrelated.path()).unwrap(),
            b"preserve this staging entry"
        );
        assert_eq!(
            fs::read(source.path()).unwrap(),
            b"new source at the old path"
        );

        fs::remove_file(unrelated.path()).unwrap();
        let (mut progress, mut view) = Progress::tracked_transfer();
        let result = resume(&recovery, &offer.id, &mut progress).unwrap();
        assert!(result.moving);
        assert_eq!(result.kind, Completion::AlreadyPresent);
        assert_eq!(progress.results.items[0].outcome, Outcome::Completed);
        assert_eq!(view.snapshot().total_written, 0);
        assert!(scan(&recovery).0.is_empty());
        assert!(!unrelated.parent().unwrap().path().exists());
        assert_eq!(
            fs::read(source.path()).unwrap(),
            b"new source at the old path"
        );
        assert_eq!(
            fs::metadata(target.path()).unwrap().ino(),
            original_target.ino()
        );
        assert_eq!(fs::read(target.path()).unwrap(), b"original data");
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
        let TransferError::Failed(Failure::Mutation(incomplete)) = &error else {
            panic!("Expected an incomplete move, got {error:?}");
        };
        let MutationError::MoveIncomplete {
            target: published,
            cause,
        } = incomplete.as_ref()
        else {
            panic!("Expected an incomplete move, got {incomplete:?}");
        };
        assert_eq!(published, &target);
        let TransferError::Failed(Failure::Mutation(retained)) = cause else {
            panic!("Expected a retained original, got {cause:?}");
        };
        let MutationError::RetainedMoveSource { held: kept, cause } = retained.as_ref() else {
            panic!("Expected a retained original, got {retained:?}");
        };
        assert_eq!(kept, &held);
        let (role, location, reason) = match problem {
            "unexpected sibling" => (ResumeRole::Staged, &sibling, ResumeCheckProblem::Unexpected),
            "legacy receipt" => (
                ResumeRole::HeldOriginal,
                &held,
                ResumeCheckProblem::MissingProof,
            ),
            _ => (ResumeRole::HeldOriginal, &held, ResumeCheckProblem::Changed),
        };
        assert_resume_check(cause, role, location.path(), reason);
        let message = crate::i18n::file_error(&error);
        assert!(message.contains(&held.display()), "{problem}: {message}");
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
    let result = resume(recovery, &offers[0].id, &mut progress).unwrap();
    let reused = suffix_offset + HEAD.len() as u64;
    assert_eq!(result.kind, Completion::FolderCopied(reused));
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
fn recovery_move_budget_counts_ancestors_and_preserves_the_nested_path() {
    let temp = tempfile::tempdir().unwrap();
    let source: Location = temp.path().canonicalize().unwrap().into();
    let nested = source.join("nested");
    let child = nested.join("child");
    fs::create_dir(nested.path()).unwrap();
    fs::write(source.join("a").path(), b"first").unwrap();
    fs::write(child.path(), b"nested").unwrap();
    let cancel = Cancellation::default();
    let error = TransferSourceManifest::capture_recovery_tree(&source, &cancel, true, Some(3))
        .err()
        .expect("the root and its nested directory also consume journal entries");
    assert!(matches!(
        &error,
        TransferError::Failed(Failure::Scan(error))
            if matches!(error.as_ref(), ScanError::MoveEntryLimit { location, limit: 3 }
                if *location == child)
    ));
    assert!(crate::i18n::file_error(&error).contains(&child.display()));
    for limit in [Some(4), None] {
        let manifest =
            TransferSourceManifest::capture_recovery_tree(&source, &cancel, true, limit).unwrap();
        assert_eq!(manifest.entries.len(), 4);
        assert_eq!(manifest.bytes, 11);
    }
    cancel.request();
    assert!(matches!(
        TransferSourceManifest::capture_recovery_tree(&source, &cancel, true, Some(3)),
        Err(TransferError::Cancelled)
    ));
}

#[test]
fn changed_published_folder_reports_the_destination_without_a_retained_draft() {
    for moving in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let recovery = root.join("recovery");
        let source: Location = root.join("source").into();
        let target: Location = root.join("target").into();
        fs::create_dir(source.path()).unwrap();
        fs::write(source.join("data").path(), b"original").unwrap();
        let item = TransferItem::new(source.clone(), source.clone(), target.clone());
        let offer = if moving {
            interrupt_move(&item, &recovery, Phase::Copied)
        } else {
            let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                copy_folder(&item, &recovery, &mut Progress::default(), || {
                    panic!("interrupted just after publication")
                })
                .unwrap();
            }));
            assert_eq!(
                stopped.unwrap_err().downcast_ref::<&str>(),
                Some(&"interrupted just after publication")
            );
            let (mut offers, errors) = scan(&recovery);
            assert_eq!(offers.len(), 1, "{errors:?}");
            offers.pop().unwrap()
        };
        let record = receipts(&recovery).join(&offer.id);
        let ticket = Ticket::open(record.clone()).unwrap().unwrap();
        let stage = ticket.paths().unwrap().stage;
        assert!(!stage.join("payload").path().exists());
        drop(ticket);
        fs::write(target.join("data").path(), b"new destination data").unwrap();
        let (mut progress, mut view) = Progress::tracked_transfer();
        let error = resume(&recovery, &offer.id, &mut progress).unwrap_err();
        assert_resume_check(
            &error,
            ResumeRole::Destination,
            target.path(),
            ResumeCheckProblem::ContentsMismatch,
        );
        assert!(!progress.partial_copy_retained);
        assert_eq!(view.snapshot().total_written, 0);
        assert_eq!(progress.results.items[0].outcome, Outcome::Failed);
        assert!(record.exists() && stage.path().exists());
        assert_eq!(fs::read(source.join("data").path()).unwrap(), b"original");
        assert_eq!(
            fs::read(target.join("data").path()).unwrap(),
            b"new destination data"
        );
    }
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
            let result = resume(&recovery, &offers[0].id, &mut progress).unwrap();
            assert_eq!(result.kind, Completion::FolderCopied(saved));
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
        let error = resume(&recovery, &offers[0].id, &mut progress).unwrap_err();
        let (role, location, problem) = match change {
            "source" => (
                ResumeRole::Source,
                &source,
                ResumeCheckProblem::ContentsMismatch,
            ),
            "target" => (
                ResumeRole::Destination,
                &target,
                ResumeCheckProblem::Unexpected,
            ),
            "foreign" => (ResumeRole::Staged, &changed, ResumeCheckProblem::Unexpected),
            _ => (
                ResumeRole::Staged,
                &changed,
                ResumeCheckProblem::ContentsMismatch,
            ),
        };
        let check = retained_check(&error, stage.parent().unwrap());
        assert_resume_check(check, role, location, problem);
        assert_eq!(
            progress.results.items[0].outcome,
            Outcome::PartialCopyRetained
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
        let pending: Location = root.join("after").into();
        let output = root.join("destination");
        fs::create_dir(&output).unwrap();
        setup_folder(&source, &tail);
        fs::write(pending.path(), b"pending source").unwrap();
        fs::write(output.join("existing"), b"existing target").unwrap();
        let observed = output.clone();
        let (mut progress, _) = Progress::tracked_transfer();
        progress.resume_directory = Some(recovery.clone());
        progress.cancellation().set_checkpoint(move || {
            let ready = tail_progress(&observed).is_some_and(|len| len >= 128 * 1024);
            if ready && foreign {
                let draft = staged_payload(&observed).unwrap();
                fs::write(draft.join("outside-data"), b"preserve me").unwrap();
            }
            ready
        });
        let original: Location = source.clone().into();
        let selected = [original.clone(), pending.clone()];
        let result = transfer_with_conflicts(
            &selected,
            &output.clone().into(),
            TransferKind::Copy,
            &mut progress,
            |_| panic!("unexpected conflict"),
            |_, _| panic!("a copy must leave the source in place"),
        );
        assert!(progress.results.cancelled);
        assert_eq!(progress.results.items[1].outcome, Outcome::NotStarted);
        assert_eq!(fs::read(source.join("a-finished")).unwrap(), HEAD);
        assert_eq!(fs::read(source.join("z-child/large")).unwrap(), tail);
        assert_eq!(
            fs::read(output.join("existing")).unwrap(),
            b"existing target"
        );
        assert!(!output.join("tree").exists());
        assert!(!output.join("after").exists());
        if foreign {
            let failure = result.unwrap_err();
            assert!(failure.summary.cancelled);
            assert_eq!(failure.summary.completed, 0);
            assert_eq!(failure.summary.remaining, selected);
            let draft = staged_payload(&output).unwrap();
            let obstacle = draft.join("outside-data");
            let TransferError::Failed(Failure::StagingCleanup(error)) = failure.error else {
                panic!("expected the cancellation and cleanup error together");
            };
            assert!(matches!(error.operation, Some(TransferError::Cancelled)));
            assert_eq!(error.staging.path(), draft.parent().unwrap());
            assert_eq!(error.target.path(), output.join("tree"));
            assert_resume_check(
                &error.cleanup,
                ResumeRole::Staged,
                &obstacle,
                ResumeCheckProblem::Unexpected,
            );
            assert_eq!(fs::read(obstacle).unwrap(), b"preserve me");
            assert_eq!(fs::read(draft.join("a-finished")).unwrap(), HEAD);
            assert!(progress.partial_copy_retained);
            assert_eq!(
                progress.results.items[0].outcome,
                Outcome::PartialCopyRetained
            );
            assert_eq!(scan(&recovery).0.len(), 1);
        } else {
            let summary = result.unwrap();
            assert!(summary.cancelled);
            assert_eq!(summary.completed, 0);
            assert_eq!(progress.results.items[0].outcome, Outcome::Cancelled);
            assert!(!progress.partial_copy_retained);
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
