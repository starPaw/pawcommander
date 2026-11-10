//! A worker publishes only the latest progress snapshot. The UI never waits
//! for I/O or drains a queue of progress messages.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const UPDATE_INTERVAL: Duration = Duration::from_millis(100);

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
    view: Option<Arc<Mutex<Snapshot>>>,
    current: Snapshot,
    last_update: Option<Instant>,
}

impl Progress {
    pub fn tracked() -> (Self, ProgressView) {
        let view = Arc::new(Mutex::new(Snapshot::default()));
        (
            Self {
                view: Some(view.clone()),
                ..Self::default()
            },
            ProgressView {
                shared: view,
                latest: Snapshot::default(),
            },
        )
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
        if self.view.is_none() {
            return io::copy(input, output);
        }
        self.copy_tracked(path, expected, |progress| {
            io::copy(input, &mut ObservedWriter { output, progress })
        })
    }

    pub fn copy_local(
        &mut self,
        path: &Path,
        expected: u64,
        input: &mut File,
        output: &mut File,
    ) -> io::Result<u64> {
        if self.view.is_none() {
            return io::copy(&mut input.take(expected), output);
        }
        // Keep std's file-to-file fast path available (e.g. copy_file_range on
        // Linux), while still publishing progress during large local copies.
        const CHUNK: u64 = 1024 * 1024;
        self.copy_tracked(path, Some(expected), |progress| {
            let mut written = 0;
            while written < expected {
                let limit = CHUNK.min(expected - written);
                let chunk = io::copy(&mut (&mut *input).take(limit), output)?;
                written += chunk;
                progress.wrote(chunk);
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

struct ObservedWriter<'a, W> {
    output: &'a mut W,
    progress: &'a mut Progress,
}

impl<W: Write> Write for ObservedWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.output.write(bytes)?;
        self.progress.wrote(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
