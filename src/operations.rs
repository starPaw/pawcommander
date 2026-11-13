//! A worker publishes only the latest progress snapshot. The UI never waits
//! for I/O or drains a queue of progress messages.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const UPDATE_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(test)]
type CancellationCheckpoint = Arc<Mutex<Option<Box<dyn FnMut() -> bool + Send>>>>;

/// A request is independent of progress publication and remains valid without a view.
#[derive(Clone, Default)]
pub(crate) struct Cancellation {
    requested: Arc<AtomicBool>,
    #[cfg(test)]
    checkpoint: CancellationCheckpoint,
}

#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("transfer cancelled")
    }
}
impl std::error::Error for Cancelled {}

impl Cancellation {
    pub fn request(&self) {
        self.requested.store(true, Ordering::Relaxed);
    }

    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn at_checkpoint(checkpoint: impl FnMut() -> bool + Send + 'static) -> Self {
        Self {
            checkpoint: Arc::new(Mutex::new(Some(Box::new(checkpoint)))),
            ..Self::default()
        }
    }

    pub fn check(&self) -> io::Result<()> {
        #[cfg(test)]
        if self
            .checkpoint
            .lock()
            .unwrap()
            .as_mut()
            .is_some_and(|check| check())
        {
            self.request();
        }
        if self.is_requested() {
            Err(io::Error::other(Cancelled))
        } else {
            Ok(())
        }
    }
}

/// Keep cancellation typed until private staging has been cleaned. String
/// errors always represent real failures, including cancellation cleanup errors.
#[derive(Debug)]
pub(crate) enum TransferError {
    Cancelled,
    Failed(String),
}

impl TransferError {
    pub fn is_cancelled_io(error: &io::Error) -> bool {
        error.get_ref().is_some_and(|e| e.is::<Cancelled>())
    }

    pub fn io(error: io::Error, context: impl FnOnce(io::Error) -> String) -> Self {
        if Self::is_cancelled_io(&error) {
            Self::Cancelled
        } else {
            Self::Failed(context(error))
        }
    }
}

impl From<io::Error> for TransferError {
    fn from(error: io::Error) -> Self {
        Self::io(error, |error| error.to_string())
    }
}
impl From<String> for TransferError {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}
impl From<&str> for TransferError {
    fn from(error: &str) -> Self {
        Self::Failed(error.into())
    }
}
impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str(&crate::tr_format!(
                "Операция отменена",
                "Operation cancelled"
            )),
            Self::Failed(error) => f.write_str(error),
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub item: Option<PathBuf>,
    pub waiting_for_conflict: bool,
    pub transfer: Option<Transfer>,
    pub total_written: u64,
}

#[derive(Clone)]
pub(crate) struct Transfer {
    pub written: u64,
    pub expected: Option<u64>,
    started: Instant,
    updated: Instant,
    pub stream_finished: bool,
}

impl Transfer {
    pub fn bytes_per_second(&self, now: Instant) -> Option<f64> {
        let elapsed = self.updated.duration_since(self.started);
        if self.stream_finished
            || self.written == 0
            || elapsed < UPDATE_INTERVAL
            || now.saturating_duration_since(self.updated) > Duration::from_secs(2)
        {
            return None;
        }
        Some(self.written as f64 / elapsed.as_secs_f64())
    }

    pub fn remaining_seconds(&self, now: Instant) -> Option<u64> {
        let remaining = self.expected?.checked_sub(self.written)?;
        Some((remaining as f64 / self.bytes_per_second(now)?).ceil() as u64)
    }
}

pub(crate) struct ProgressView {
    pub cancel: Cancellation,
    shared: Arc<Mutex<Snapshot>>,
    latest: Snapshot,
}

impl ProgressView {
    pub fn snapshot(&mut self) -> &Snapshot {
        if let Ok(snapshot) = self.shared.try_lock() {
            self.latest.clone_from(&snapshot);
        }
        &self.latest
    }
}

#[derive(Default)]
pub(crate) struct Progress {
    pub results: crate::result_history::Results,
    cancel: Cancellation,
    view: Option<Arc<Mutex<Snapshot>>>,
    current: Snapshot,
    last_update: Option<Instant>,
}

impl Progress {
    pub fn tracked() -> (Self, ProgressView) {
        let view = Arc::new(Mutex::new(Snapshot::default()));
        let cancel = Cancellation::default();
        (
            Self {
                view: Some(view.clone()),
                cancel: cancel.clone(),
                ..Self::default()
            },
            ProgressView {
                cancel,
                shared: view,
                latest: Snapshot::default(),
            },
        )
    }

    #[cfg(test)]
    pub fn with_cancellation(cancel: Cancellation) -> Self {
        Self {
            cancel,
            ..Self::default()
        }
    }

    pub fn cancellation(&self) -> Cancellation {
        self.cancel.clone()
    }

    pub fn check_cancelled(&self) -> io::Result<()> {
        self.cancel.check()
    }

    pub fn item(&mut self, path: &Path) {
        self.set_item(path, false);
    }

    pub fn waiting_for_conflict(&mut self, path: &Path) {
        self.set_item(path, true);
    }

    fn set_item(&mut self, path: &Path, waiting_for_conflict: bool) {
        if self.view.is_some() {
            self.current.item = Some(path.to_owned());
            self.current.waiting_for_conflict = waiting_for_conflict;
            self.current.transfer = None;
            self.publish(Instant::now(), true);
        }
    }

    fn publish(&mut self, now: Instant, force: bool) {
        if !force
            && self
                .last_update
                .is_some_and(|last| now.duration_since(last) < UPDATE_INTERVAL)
        {
            return;
        }
        if let Some(view) = &self.view {
            if let Ok(mut snapshot) = view.lock() {
                *snapshot = self.current.clone();
                self.last_update = Some(now);
            }
        }
    }

    pub fn copy<R: Read, W: Write>(
        &mut self,
        path: &Path,
        expected: Option<u64>,
        input: &mut R,
        output: &mut W,
    ) -> io::Result<u64> {
        self.copy_tracked(path, expected, |progress| {
            let mut buffer = [0; 64 * 1024];
            let mut written = 0;
            loop {
                progress.check_cancelled()?;
                let read = match input.read(&mut buffer) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => result?,
                };
                progress.check_cancelled()?;
                if read == 0 {
                    return Ok(written);
                }
                let mut pending = &buffer[..read];
                while !pending.is_empty() {
                    progress.check_cancelled()?;
                    let count = match output.write(pending) {
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        result => result?,
                    };
                    if count == 0 {
                        return Err(io::ErrorKind::WriteZero.into());
                    }
                    written += count as u64;
                    progress.wrote(count as u64);
                    pending = &pending[count..];
                }
            }
        })
    }

    pub fn copy_local(
        &mut self,
        path: &Path,
        expected: u64,
        input: &mut File,
        output: &mut File,
    ) -> io::Result<u64> {
        // Keep std's file-to-file fast path available (e.g. copy_file_range on
        // Linux), while still publishing progress during large local copies.
        const CHUNK: u64 = 1024 * 1024;
        self.copy_tracked(path, Some(expected), |progress| {
            let mut written = 0;
            progress.check_cancelled()?;
            while written < expected {
                progress.check_cancelled()?;
                let limit = CHUNK.min(expected - written);
                let chunk = io::copy(&mut (&mut *input).take(limit), output)?;
                written += chunk;
                progress.wrote(chunk);
                progress.check_cancelled()?;
                if chunk < limit {
                    return Ok(written);
                }
            }
            // The caller checks handle/path metadata after reading. Stopping
            // at the captured length also bounds a continuously growing file.
            Ok(written)
        })
    }

    fn copy_tracked(
        &mut self,
        path: &Path,
        expected: Option<u64>,
        copy: impl FnOnce(&mut Self) -> io::Result<u64>,
    ) -> io::Result<u64> {
        let now = Instant::now();
        self.current.item = Some(path.to_owned());
        self.current.waiting_for_conflict = false;
        self.current.transfer = Some(Transfer {
            written: 0,
            expected,
            started: now,
            updated: now,
            stream_finished: false,
        });
        self.publish(now, true);
        let result = copy(self);
        if let Some(transfer) = &mut self.current.transfer {
            transfer.stream_finished = result.is_ok();
        }
        self.publish(Instant::now(), true);
        result
    }

    fn wrote(&mut self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let now = Instant::now();
        let mut first_write = false;
        if let Some(transfer) = &mut self.current.transfer {
            first_write = transfer.written == 0;
            transfer.written = transfer.written.saturating_add(bytes);
            transfer.updated = now;
        }
        self.current.total_written = self.current.total_written.saturating_add(bytes);
        self.publish(now, first_write);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_cancellation_checks_reads_and_writes_without_a_progress_observer() {
        use std::io::Cursor;
        struct RequestOnRead {
            cancel: Cancellation,
            reads: usize,
        }
        impl Read for RequestOnRead {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                bytes[0] = 7;
                self.cancel.request();
                Ok(1)
            }
        }
        struct RequestOnWrite {
            cancel: Cancellation,
            writes: usize,
        }
        impl Write for RequestOnWrite {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                self.cancel.request();
                Ok(1)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        for tracked in [false, true] {
            let mut progress = if tracked {
                Progress::tracked().0
            } else {
                Progress::default()
            };
            let cancel = progress.cancellation();
            let mut input = RequestOnRead { cancel, reads: 0 };
            let mut output = Vec::new();
            let error = progress
                .copy(Path::new("input"), None, &mut input, &mut output)
                .unwrap_err();
            assert!(matches!(
                TransferError::from(error),
                TransferError::Cancelled
            ));
            assert_eq!(input.reads, 1);
            assert!(
                output.is_empty(),
                "Stop during read prevents the next write"
            );

            let mut progress = if tracked {
                Progress::tracked().0
            } else {
                Progress::default()
            };
            let mut output = RequestOnWrite {
                cancel: progress.cancellation(),
                writes: 0,
            };
            let mut input = Cursor::new(vec![1; 128 * 1024]);
            let error = progress
                .copy(Path::new("input"), None, &mut input, &mut output)
                .unwrap_err();
            assert!(matches!(
                TransferError::from(error),
                TransferError::Cancelled
            ));
            assert_eq!(
                output.writes, 1,
                "Stop during partial write prevents the next write"
            );
            assert_eq!(input.position(), 64 * 1024);
        }
    }

    #[test]
    fn stream_retries_real_interrupted_errors_and_observes_stop_before_retry() {
        struct Reader {
            attempts: usize,
            cancel: Option<Cancellation>,
        }
        impl Read for Reader {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.attempts += 1;
                if self.attempts == 1 {
                    if let Some(cancel) = &self.cancel {
                        cancel.request();
                    }
                    return Err(io::ErrorKind::Interrupted.into());
                }
                if self.attempts == 2 {
                    bytes[0] = 9;
                    Ok(1)
                } else {
                    Ok(0)
                }
            }
        }
        let mut progress = Progress::default();
        let mut reader = Reader {
            attempts: 0,
            cancel: None,
        };
        let mut output = Vec::new();
        assert_eq!(
            progress
                .copy(Path::new("source"), None, &mut reader, &mut output)
                .unwrap(),
            1
        );
        assert_eq!(output, vec![9]);
        let mut reader = Reader {
            attempts: 0,
            cancel: Some(progress.cancellation()),
        };
        let error = progress
            .copy(Path::new("source"), None, &mut reader, &mut Vec::new())
            .unwrap_err();
        assert!(matches!(
            TransferError::from(error),
            TransferError::Cancelled
        ));
        assert_eq!(reader.attempts, 1);
    }

    #[test]
    fn cancelled_stream_never_starts_even_for_empty_inputs() {
        for tracked in [false, true] {
            let mut progress = if tracked {
                Progress::tracked().0
            } else {
                Progress::default()
            };
            progress.cancellation().request();
            let mut input = std::io::Cursor::new(Vec::<u8>::new());
            let mut output = Vec::new();
            let error = progress
                .copy(Path::new("empty"), Some(0), &mut input, &mut output)
                .unwrap_err();
            assert!(matches!(
                TransferError::from(error),
                TransferError::Cancelled
            ));
            let mut source = tempfile::tempfile().unwrap();
            let mut target = tempfile::tempfile().unwrap();
            let error = progress
                .copy_local(Path::new("empty"), 0, &mut source, &mut target)
                .unwrap_err();
            assert!(matches!(
                TransferError::from(error),
                TransferError::Cancelled
            ));
            assert!(output.is_empty());
        }
    }

    #[test]
    fn local_stream_checks_between_fast_path_chunks() {
        use std::io::{Seek, Write};
        let mut source = tempfile::tempfile().unwrap();
        source.write_all(&vec![42; 3 * 1024 * 1024]).unwrap();
        source.rewind().unwrap();
        let mut target = tempfile::tempfile().unwrap();
        let observed_target = target.try_clone().unwrap();
        let cancel =
            Cancellation::at_checkpoint(move || observed_target.metadata().unwrap().len() != 0);
        let mut progress = Progress::with_cancellation(cancel);
        let error = progress
            .copy_local(
                Path::new("source"),
                3 * 1024 * 1024,
                &mut source,
                &mut target,
            )
            .unwrap_err();
        assert!(matches!(
            TransferError::from(error),
            TransferError::Cancelled
        ));
        assert_eq!(target.metadata().unwrap().len(), 1024 * 1024);
        assert_eq!(source.stream_position().unwrap(), 1024 * 1024);
    }

    #[test]
    fn local_copy_stops_at_captured_length_even_if_source_has_grown() {
        use std::io::{Seek, Write};
        for tracked in [false, true] {
            for expected in [0, 5, 1024 * 1024 + 3] {
                let mut input = tempfile::tempfile().unwrap();
                input.write_all(&vec![0x5a; expected as usize]).unwrap();
                let captured = input.metadata().unwrap().len();
                input.write_all(b"growth after capture").unwrap();
                input.rewind().unwrap();
                let mut output = tempfile::tempfile().unwrap();
                let mut progress = if tracked {
                    Progress::tracked().0
                } else {
                    Progress::default()
                };
                let written = progress
                    .copy_local(Path::new("source"), captured, &mut input, &mut output)
                    .unwrap();
                assert_eq!(written, captured);
                assert_eq!(output.metadata().unwrap().len(), captured);
                assert_eq!(input.stream_position().unwrap(), captured);
                assert!(input.metadata().unwrap().len() > captured);
            }
        }
    }

    #[test]
    fn estimates_only_when_size_and_recent_rate_are_known() {
        let started = Instant::now();
        let updated = started + Duration::from_secs(2);
        let mut transfer = Transfer {
            written: 100,
            expected: Some(225),
            started,
            updated,
            stream_finished: false,
        };
        assert_eq!(transfer.bytes_per_second(updated), Some(50.0));
        assert_eq!(transfer.remaining_seconds(updated), Some(3));
        assert_eq!(
            transfer.bytes_per_second(updated + Duration::from_secs(3)),
            None
        );
        transfer.expected = Some(99);
        assert_eq!(transfer.remaining_seconds(updated), None);
        transfer.expected = None;
        assert_eq!(transfer.remaining_seconds(updated), None);
        transfer.stream_finished = true;
        assert_eq!(transfer.bytes_per_second(updated), None);
    }
}
