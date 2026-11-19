use super::*;
use crate::operations::{PauseState, ProgressView};
use std::{io::Write, sync::mpsc, thread, time::Instant};

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PauseAction {
    Resume,
    Stop,
    ChangeSource,
}

pub(super) fn check_copy_pause(
    source_root: &Location,
    target_root: &Location,
    name: &str,
    action: PauseAction,
    verify: bool,
) {
    let from = create_directory(source_root, &format!("{name}-from")).unwrap();
    let to = create_directory(target_root, &format!("{name}-to")).unwrap();
    let source = from.join("file.bin");
    let target = to.join("file.bin");
    let data: Vec<_> = (0..4)
        .flat_map(|byte| std::iter::repeat_n(byte, 1024 * 1024))
        .collect();
    let size = data.len() as u64;
    fs::write(source.path(), &data).unwrap();
    fs::write(target.path(), b"previous destination").unwrap();

    let (mut progress, mut view) = Progress::tracked_transfer();
    progress.set_copy_verification(verify);
    let (partial, partial_ready) = mpsc::channel();
    let (release, checkpoint) = mpsc::channel();
    let inspect = to.path().to_path_buf();
    let mut observed_partial = false;
    progress.cancellation().set_checkpoint(move || {
        if observed_partial {
            return false;
        }
        for entry in fs::read_dir(&inspect).unwrap() {
            let payload = entry.unwrap().path().join("payload");
            if let Ok(meta) = fs::metadata(&payload)
                && (1..size).contains(&meta.len())
            {
                observed_partial = true;
                partial.send((payload, meta.len())).unwrap();
                checkpoint.recv_timeout(TIMEOUT).unwrap();
                break;
            }
        }
        false
    });
    let copy = source.clone();
    let (done, result) = mpsc::channel();
    let worker = thread::spawn(move || {
        let report = transfer_with_conflicts(
            &[copy],
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
        );
        done.send(report).unwrap();
    });

    let (staging, initial_size) = partial_ready.recv_timeout(TIMEOUT).unwrap();
    view.set_paused(true);
    release.send(()).unwrap();
    wait_for_pause(&view);
    let snapshot = view.snapshot();
    let transfer = snapshot.transfer.as_ref().unwrap();
    let written = transfer.written;
    assert!((initial_size..size).contains(&written), "{name}: {written}");
    assert_eq!(transfer.expected, Some(size));
    let partial_bytes = fs::read(&staging).unwrap();
    assert_eq!(partial_bytes.len() as u64, written, "{name}");
    assert!(
        partial_bytes == data[..written as usize],
        "{name}: staged prefix"
    );
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(40)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(fs::metadata(&staging).unwrap().len(), written);
    assert_eq!(fs::read(target.path()).unwrap(), b"previous destination");

    let mut expected_source = data.clone();
    match action {
        PauseAction::Resume => view.set_paused(false),
        PauseAction::Stop => view.cancel.request(),
        PauseAction::ChangeSource => {
            fs::OpenOptions::new()
                .append(true)
                .open(source.path())
                .unwrap()
                .write_all(b"edit")
                .unwrap();
            expected_source.extend_from_slice(b"edit");
            view.set_paused(false);
        }
    }
    let report = result.recv_timeout(TIMEOUT).unwrap();
    worker.join().unwrap();
    match action {
        PauseAction::Resume => {
            assert_eq!(report.unwrap().completed, 1);
            assert_eq!(view.snapshot().total_written, size);
            assert!(
                fs::read(target.path()).unwrap() == data,
                "{name}: resumed contents"
            );
        }
        PauseAction::Stop => {
            let report = report.unwrap();
            assert!(report.cancelled);
            assert_eq!(report.completed, 0);
            assert_eq!(report.remaining, std::slice::from_ref(&source));
        }
        PauseAction::ChangeSource => assert!(report.is_err()),
    }
    if action != PauseAction::Resume {
        assert_eq!(fs::read(target.path()).unwrap(), b"previous destination");
    }
    assert!(
        fs::read(source.path()).unwrap() == expected_source,
        "{name}: source contents"
    );
    assert_eq!(
        fs::read_dir(target.path().parent().unwrap())
            .unwrap()
            .count(),
        1
    );
}

fn wait_for_pause(view: &ProgressView) {
    let start = Instant::now();
    while view.pause_state() != Some(PauseState::Paused) && start.elapsed() < TIMEOUT {
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(view.pause_state(), Some(PauseState::Paused));
}

#[test]
fn transfer_pause_resumes_or_stops_local_replacement_with_staging_intact() {
    let temporary = tempfile::tempdir().unwrap();
    let root = Location::Local(temporary.path().to_owned());
    for (name, action, verify) in [
        ("resume", PauseAction::Resume, false),
        ("stop", PauseAction::Stop, true),
        ("source-edit", PauseAction::ChangeSource, true),
    ] {
        check_copy_pause(&root, &root, name, action, verify);
    }
}

#[test]
fn transfer_pause_between_renames_preserves_committed_move_and_remainder() {
    let temporary = tempfile::tempdir().unwrap();
    let root = Location::Local(temporary.path().to_owned());
    let to = create_directory(&root, "destination").unwrap();
    let sources = [root.join("first"), root.join("second")];
    for source in &sources {
        fs::write(source.path(), b"original").unwrap();
    }
    let (mut progress, view) = Progress::tracked_transfer();
    let (moved, published) = mpsc::channel();
    let (release, proceed) = mpsc::channel();
    let (done, result) = mpsc::channel();
    let destination = to.clone();
    let plan = sources.clone();
    let worker = thread::spawn(move || {
        let report = transfer_with_conflicts(
            &plan,
            &destination,
            TransferKind::Move,
            &mut progress,
            |_| panic!("no conflict"),
            |item, _| {
                moved.send(item.clone()).unwrap();
                proceed.recv_timeout(TIMEOUT).unwrap();
            },
        );
        done.send(report).unwrap();
    });
    assert_eq!(published.recv_timeout(TIMEOUT).unwrap(), sources[0]);
    view.set_paused(true);
    release.send(()).unwrap();
    wait_for_pause(&view);
    assert!(!sources[0].path().exists());
    assert_eq!(fs::read(to.join("first").path()).unwrap(), b"original");
    assert!(!to.join("second").path().exists());
    view.cancel.request();
    let report = result.recv_timeout(TIMEOUT).unwrap().unwrap();
    worker.join().unwrap();
    assert!(report.cancelled);
    assert_eq!(report.completed, 1);
    assert_eq!(report.remaining, sources[1..]);
    assert_eq!(fs::read(sources[1].path()).unwrap(), b"original");
}
