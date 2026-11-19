use super::*;
use crate::operations::{PauseState, TransferLimits};
use std::{sync::mpsc, thread, time::Instant};

const TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn check_transfer_speed(
    source_root: &Location,
    target_root: &Location,
    name: &str,
    stop_after_limit_change: bool,
) {
    let from = create_directory(source_root, &format!("limit-{name}-from")).unwrap();
    let to = create_directory(target_root, &format!("limit-{name}-to")).unwrap();
    let source = from.join("payload.bin");
    let target = to.join("payload.bin");
    let data = vec![0x56; 512 * 1024];
    fs::write(source.path(), &data).unwrap();

    let endpoint = source.sftp_endpoint().or_else(|| target.sftp_endpoint());
    let (mut progress, view) = Progress::tracked_transfer();
    let mut limits = TransferLimits::default();
    let mut set_limit = |kib| {
        if let Some(endpoint) = endpoint {
            limits.global_kib = 128;
            limits.set_connection_kib(endpoint, kib);
            if kib == 0 {
                limits.global_kib = 0;
            }
        } else {
            limits.global_kib = kib;
        }
        view.set_transfer_limits(&limits);
    };
    set_limit(64);

    let selection = [source.clone()];
    let destination = to.clone();
    let (done, result) = mpsc::channel();
    let worker = thread::spawn(move || {
        let report = transfer_with_conflicts(
            &selection,
            &destination,
            TransferKind::Copy,
            &mut progress,
            |_| panic!("unexpected conflict"),
            |_, _| panic!("copy"),
        );
        done.send(report).unwrap();
    });

    let started = Instant::now();
    let staging = loop {
        if let Some(file) = fs::read_dir(to.path()).unwrap().find_map(|entry| {
            let file = entry.unwrap().path().join("payload");
            fs::metadata(&file)
                .is_ok_and(|meta| meta.len() > 0)
                .then_some(file)
        }) {
            break file;
        }
        assert!(started.elapsed() < TIMEOUT, "{name}: no staged bytes");
        assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
        thread::sleep(Duration::from_millis(2));
    };
    view.set_paused(true);
    super::pause::wait_for_pause(&view);
    let paused_at = fs::metadata(&staging).unwrap().len();
    set_limit(8);
    assert_eq!(view.pause_state(), Some(PauseState::Paused));
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(70)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(fs::metadata(&staging).unwrap().len(), paused_at);

    let resumed = Instant::now();
    view.set_paused(false);
    thread::sleep(Duration::from_millis(230));
    let actual = fs::metadata(&staging).unwrap().len() - paused_at;
    let allowed =
        64 * 1024 / 20 + (resumed.elapsed().as_secs_f64() * (8 * 1024) as f64) as u64 + 1024;
    assert!(
        (1..=allowed).contains(&actual),
        "{name}: {actual} bytes, allowed {allowed}"
    );
    assert!(!target.path().exists());

    if stop_after_limit_change {
        view.cancel.request();
        let report = result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert!(report.cancelled);
        assert_eq!(report.remaining, std::slice::from_ref(&source));
        assert!(!target.path().exists());
    } else {
        set_limit(0);
        let report = result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(report.completed, 1);
        assert!(
            fs::read(target.path()).unwrap() == data,
            "{name}: destination contents"
        );
    }
    worker.join().unwrap();
    assert!(
        fs::read(source.path()).unwrap() == data,
        "{name}: source contents"
    );
    assert!(!staging.exists());
}

#[test]
fn transfer_speed_applies_global_changes_to_an_active_local_copy() {
    let fixture = tempfile::tempdir().unwrap();
    let root = Location::Local(fixture.path().to_owned());
    check_transfer_speed(&root, &root, "local", false);
}
