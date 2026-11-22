//! A worker publishes only the latest progress snapshot. The UI never waits
//! for I/O or drains a queue of progress messages.

mod access_stream;
mod bandwidth;
pub(crate) use access_stream::{access_io, AccessStream};

use crate::file_error::{
    AccessCause, AccessError, ArchiveError, ChecksumError, ConnectionError, CopyVerificationError,
    Failure, HostKeyError, ListingError, MetadataError, MutationError, PathError, PermissionError,
    PreviewError, PublicationError, RecoveryNotice, ScanError, SearchError, SpaceError,
};
use crate::vfs::{Endpoint, Location};
use bandwidth::Pacer;
pub(crate) use bandwidth::TransferLimits;
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const UPDATE_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(test)]
type CancellationCheckpoint = Mutex<Option<Box<dyn FnMut() -> bool + Send>>>;

/// A request is independent of progress publication and remains valid without a view.
#[derive(Clone, Default)]
pub(crate) struct Cancellation {
    control: Arc<TransferControl>,
    pause_at_checks: bool,
}

#[derive(Default)]
struct TransferControl {
    requested: AtomicBool,
    transfer: Option<ActiveTransferControl>,
    #[cfg(test)]
    checkpoint: CancellationCheckpoint,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PauseState {
    #[default]
    Running,
    Requested,
    Paused,
}

#[derive(Default)]
struct ActiveTransferControl {
    state: Mutex<ActiveTransferState>,
    changed: Condvar,
}

#[derive(Default)]
struct ActiveTransferState {
    pause: PauseState,
    limits: TransferLimits,
    endpoints: [Option<Endpoint>; 2],
    copy_rate: u64,
    readback_rate: u64,
}

impl ActiveTransferState {
    fn refresh_rates(&mut self) {
        let endpoints = self.endpoints.each_ref().map(Option::as_ref);
        self.copy_rate = self.limits.copy_rate(endpoints);
        self.readback_rate = self.limits.readback_rate(endpoints[1]);
    }
}

enum CopyTraffic {
    Payload,
    Readback,
}

#[derive(Debug)]
pub(crate) struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("transfer cancelled")
    }
}
impl std::error::Error for Cancelled {}

impl Cancellation {
    fn for_transfer() -> Self {
        Self {
            control: Arc::new(TransferControl {
                transfer: Some(ActiveTransferControl::default()),
                ..TransferControl::default()
            }),
            pause_at_checks: false,
        }
    }

    pub fn request(&self) {
        self.control.requested.store(true, Ordering::Relaxed);
        if let Some(transfer) = &self.control.transfer {
            // Synchronize with the worker's decision to sleep before signaling.
            let _state = transfer.state.lock().unwrap();
            transfer.changed.notify_one();
        }
    }

    pub fn is_requested(&self) -> bool {
        self.control.requested.load(Ordering::Relaxed)
    }

    /// Let a read-only preparation scan pause at its existing checkpoints.
    pub fn pausable_scan(mut self) -> Self {
        self.pause_at_checks = self.control.transfer.is_some();
        self
    }

    #[cfg(test)]
    pub fn at_checkpoint(checkpoint: impl FnMut() -> bool + Send + 'static) -> Self {
        let cancel = Self::default();
        cancel.set_checkpoint(checkpoint);
        cancel
    }

    #[cfg(test)]
    pub fn set_checkpoint(&self, checkpoint: impl FnMut() -> bool + Send + 'static) {
        *self.control.checkpoint.lock().unwrap() = Some(Box::new(checkpoint));
    }

    pub fn check(&self) -> io::Result<()> {
        #[cfg(test)]
        if self
            .control
            .checkpoint
            .lock()
            .unwrap()
            .as_mut()
            .is_some_and(|check| check())
        {
            self.request();
        }
        self.check_requested()?;
        if self.pause_at_checks {
            self.wait_for_resume(|_| {})?;
        }
        Ok(())
    }

    pub(crate) fn read_up_to(&self, input: &mut impl Read, limit: usize) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut block = [0; 64 * 1024];
        while bytes.len() < limit {
            self.check()?;
            let count = (limit - bytes.len()).min(block.len());
            match input.read(&mut block[..count]) {
                Ok(0) => break,
                Ok(count) => bytes.extend_from_slice(&block[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        self.check()?;
        Ok(bytes)
    }

    fn check_requested(&self) -> io::Result<()> {
        if self.is_requested() {
            Err(io::Error::other(Cancelled))
        } else {
            Ok(())
        }
    }

    fn pause_state(&self) -> Option<PauseState> {
        let transfer = self.control.transfer.as_ref()?;
        Some(transfer.state.lock().unwrap().pause)
    }

    fn wait_for_resume(&self, publish: impl FnOnce(Instant)) -> io::Result<Duration> {
        let Some(transfer) = &self.control.transfer else {
            return Ok(Duration::ZERO);
        };
        let mut state = transfer.state.lock().unwrap();
        self.check_requested()?;
        if state.pause != PauseState::Requested {
            return Ok(Duration::ZERO);
        }
        let started = Instant::now();
        publish(started);
        state.pause = PauseState::Paused;
        let _state = transfer
            .changed
            .wait_while(state, |state| {
                state.pause == PauseState::Paused && !self.is_requested()
            })
            .unwrap();
        self.check_requested()?;
        Ok(started.elapsed())
    }
}

/// Keep cancellation typed until private staging has been cleaned. String
/// errors always represent real failures, including cancellation cleanup errors.
#[derive(Debug)]
pub(crate) enum TransferError {
    Cancelled,
    Failed(Failure),
    /// The server may have changed the item before its reply was lost.
    PublicationUnconfirmed(Failure),
}

impl TransferError {
    #[cfg(target_os = "macos")]
    pub(crate) fn is_completed_cleanup_warning(&self) -> bool {
        match self {
            Self::Failed(Failure::StagingCleanup(error)) => error.operation.is_none(),
            Self::Failed(Failure::Publication(error)) => {
                matches!(
                    error.as_ref(),
                    PublicationError::RecoveryFinalization { .. }
                )
            }
            _ => false,
        }
    }

    pub(crate) fn with_unconfirmed_publication(self) -> Self {
        match self {
            Self::Failed(reason) => Self::PublicationUnconfirmed(reason),
            error => error,
        }
    }

    pub(crate) fn map_occupied_rename(self, conflict: impl FnOnce(io::Error) -> Self) -> Self {
        match self {
            Self::Failed(Failure::Mutation(error)) => match *error {
                MutationError::RenameLocal { source, .. }
                    if source.kind() == io::ErrorKind::AlreadyExists =>
                {
                    conflict(source)
                }
                error => error.into(),
            },
            error => error,
        }
    }

    pub(crate) fn with_publication_context(
        self,
        context: impl FnOnce(Self) -> PublicationError,
    ) -> Self {
        match self {
            Self::Cancelled => self,
            Self::Failed(_) => Self::Failed(context(self).into()),
            Self::PublicationUnconfirmed(_) => Self::PublicationUnconfirmed(context(self).into()),
        }
    }

    pub(crate) fn with_retained_staging(self, staging: &Location, notice: RecoveryNotice) -> Self {
        self.with_publication_context(|cause| PublicationError::Retained {
            staging: staging.clone(),
            notice,
            cause,
        })
    }

    pub fn is_cancelled_io(error: &io::Error) -> bool {
        error.get_ref().is_some_and(|e| e.is::<Cancelled>())
    }

    pub fn io(error: io::Error, context: impl FnOnce(&io::Error) -> String) -> Self {
        if Self::is_cancelled_io(&error) {
            return Self::Cancelled;
        }
        let error = match error.downcast::<AccessError>() {
            Ok(error) => return error.into(),
            Err(error) => error,
        };
        match error.downcast::<Failure>() {
            Ok(reason) => Self::Failed(reason),
            Err(error) => Self::Failed(Failure::Io {
                message: context(&error),
                source: error,
            }),
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
        Self::Failed(Failure::Message(error))
    }
}
impl From<&str> for TransferError {
    fn from(error: &str) -> Self {
        Self::from(error.to_owned())
    }
}

impl From<PathError> for TransferError {
    fn from(error: PathError) -> Self {
        Self::Failed(Failure::Path(error))
    }
}

impl From<AccessError> for TransferError {
    fn from(error: AccessError) -> Self {
        match &error.cause {
            AccessCause::Local(source) | AccessCause::SftpStream(source)
                if Self::is_cancelled_io(source) =>
            {
                Self::Cancelled
            }
            AccessCause::Connection(source) if matches!(source.as_ref(), Self::Cancelled) => {
                Self::Cancelled
            }
            _ => Self::Failed(Failure::Access(Box::new(error))),
        }
    }
}

impl From<PreviewError> for TransferError {
    fn from(error: PreviewError) -> Self {
        Self::Failed(Failure::Preview(Box::new(error)))
    }
}

impl From<SearchError> for TransferError {
    fn from(error: SearchError) -> Self {
        Self::Failed(Failure::Search(Box::new(error)))
    }
}

impl From<ScanError> for TransferError {
    fn from(error: ScanError) -> Self {
        Self::Failed(error.into())
    }
}

impl From<PermissionError> for TransferError {
    fn from(error: PermissionError) -> Self {
        Self::Failed(Failure::Permissions(Box::new(error)))
    }
}

impl From<ChecksumError> for TransferError {
    fn from(error: ChecksumError) -> Self {
        Self::Failed(Failure::Checksum(Box::new(error)))
    }
}

impl From<ArchiveError> for TransferError {
    fn from(error: ArchiveError) -> Self {
        if error.io_cause().is_some_and(Self::is_cancelled_io) {
            Self::Cancelled
        } else {
            Self::Failed(Failure::Archive(Box::new(error)))
        }
    }
}

impl From<MetadataError> for TransferError {
    fn from(error: MetadataError) -> Self {
        Self::Failed(Failure::Metadata(Box::new(error)))
    }
}

impl From<SpaceError> for TransferError {
    fn from(error: SpaceError) -> Self {
        Self::Failed(Failure::Space(Box::new(error)))
    }
}

#[cfg(target_os = "macos")]
impl From<crate::file_error::SyncError> for TransferError {
    fn from(error: crate::file_error::SyncError) -> Self {
        Self::Failed(Failure::Sync(Box::new(error)))
    }
}

impl From<crate::file_error::StagingError> for TransferError {
    fn from(error: crate::file_error::StagingError) -> Self {
        Self::Failed(Failure::Staging(Box::new(error)))
    }
}

impl From<crate::file_error::RecoveryRecordError> for TransferError {
    fn from(error: crate::file_error::RecoveryRecordError) -> Self {
        Self::Failed(Failure::RecoveryRecord(Box::new(error)))
    }
}

impl From<ConnectionError> for TransferError {
    fn from(error: ConnectionError) -> Self {
        Self::Failed(Failure::Connection(Box::new(error)))
    }
}

impl From<HostKeyError> for TransferError {
    fn from(error: HostKeyError) -> Self {
        ConnectionError::HostKey(error).into()
    }
}

impl From<ListingError> for TransferError {
    fn from(error: ListingError) -> Self {
        Self::Failed(Failure::Listing(Box::new(error)))
    }
}

impl From<MutationError> for TransferError {
    fn from(error: MutationError) -> Self {
        Self::Failed(error.into())
    }
}

impl From<PublicationError> for TransferError {
    fn from(error: PublicationError) -> Self {
        Self::Failed(error.into())
    }
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str(&crate::tr_format!("Operation cancelled")),
            Self::Failed(error) | Self::PublicationUnconfirmed(error) => {
                std::fmt::Display::fmt(error, f)
            }
        }
    }
}

impl std::error::Error for TransferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Failed(error) | Self::PublicationUnconfirmed(error) => {
                std::error::Error::source(error)
            }
            Self::Cancelled => None,
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub item: Option<PathBuf>,
    pub waiting_for_conflict: bool,
    pub transfer: Option<Transfer>,
    pub verification: Option<Verification>,
    pub total_written: u64,
    pub reading_inputs: bool,
}

#[derive(Clone)]
pub(crate) struct Verification {
    pub read: u64,
    pub expected: u64,
}

pub(crate) enum HashRead {
    Complete([u8; 32]),
    Short(u64),
    ExtraData,
}

impl HashRead {
    fn digest(self) -> Option<[u8; 32]> {
        match self {
            Self::Complete(digest) => Some(digest),
            Self::Short(_) | Self::ExtraData => None,
        }
    }
}

/// Hash the exact bounded source stream consumed by the copy. Disabled copies
/// avoid hashing, and local copies retain their separate file-to-file fast path.
pub(crate) struct CopyReader<R> {
    inner: R,
    hash: Option<Sha256>,
}

impl<R> CopyReader<R> {
    pub fn new(inner: R, verify: bool) -> Self {
        Self {
            inner,
            hash: verify.then(Sha256::new),
        }
    }

    pub fn finish(self) -> Option<[u8; 32]> {
        self.hash.map(|hash| hash.finalize().into())
    }
}

impl<R: Read> Read for CopyReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if let Some(hash) = &mut self.hash {
            hash.update(&buffer[..read]);
        }
        Ok(read)
    }
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
    pub fn pause_state(&self) -> Option<PauseState> {
        self.cancel.pause_state()
    }

    pub fn set_paused(&self, paused: bool) {
        let Some(transfer) = &self.cancel.control.transfer else {
            return;
        };
        let mut state = transfer.state.lock().unwrap();
        if self.cancel.is_requested() {
            return;
        }
        state.pause = match (state.pause, paused) {
            (PauseState::Running, true) => PauseState::Requested,
            (PauseState::Requested | PauseState::Paused, false) => PauseState::Running,
            _ => return,
        };
        transfer.changed.notify_one();
    }

    pub fn set_transfer_limits(&self, limits: &TransferLimits) {
        if let Some(transfer) = &self.cancel.control.transfer {
            let mut state = transfer.state.lock().unwrap();
            state.limits = limits.clone();
            state.refresh_rates();
            transfer.changed.notify_one();
        }
    }

    pub fn transfer_endpoints(&self) -> [Option<Endpoint>; 2] {
        self.cancel
            .control
            .transfer
            .as_ref()
            .map_or_else(Default::default, |transfer| {
                transfer.state.lock().unwrap().endpoints.clone()
            })
    }

    pub fn snapshot(&mut self) -> &Snapshot {
        if let Ok(snapshot) = self.shared.try_lock() {
            self.latest.clone_from(&snapshot);
        }
        &self.latest
    }
}

#[derive(Default)]
pub(crate) struct Progress {
    #[cfg(target_os = "macos")]
    pub resume_directory: Option<PathBuf>,
    #[cfg(target_os = "macos")]
    pub partial_copy_retained: bool,
    pub results: crate::result_history::Results,
    pub retry: Option<crate::vfs::RetryTransfer>,
    pub undo: crate::vfs::UndoBatch,
    cancel: Cancellation,
    view: Option<Arc<Mutex<Snapshot>>>,
    current: Snapshot,
    last_update: Option<Instant>,
    verify_copies: bool,
    pacer: Pacer,
}

impl Progress {
    pub fn tracked() -> (Self, ProgressView) {
        Self::track(Cancellation::default())
    }

    pub fn tracked_transfer() -> (Self, ProgressView) {
        Self::track(Cancellation::for_transfer())
    }

    fn track(cancel: Cancellation) -> (Self, ProgressView) {
        let view = Arc::new(Mutex::new(Snapshot::default()));
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

    /// Capture a submitted transfer's option once, before any queued work runs.
    /// Rename-only moves do not consume this option or claim verification.
    pub fn set_copy_verification(&mut self, enabled: bool) {
        self.verify_copies = enabled;
    }

    pub fn copy_verification(&self) -> bool {
        self.verify_copies
    }

    pub fn set_copy_endpoints(&mut self, source: &Location, target: &Location) {
        if let Some(transfer) = &self.cancel.control.transfer {
            let mut state = transfer.state.lock().unwrap();
            state.endpoints = [source, target].map(|location| location.sftp_endpoint().cloned());
            state.refresh_rates();
        }
    }

    pub fn check_cancelled(&self) -> io::Result<()> {
        self.cancel.check()
    }

    /// Pause only between I/O calls while the worker still owns private state.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        self.check_cancelled()?;
        if self.cancel.pause_state() == Some(PauseState::Requested) {
            let cancel = self.cancel.clone();
            let elapsed = cancel.wait_for_resume(|now| self.publish(now, true))?;
            if let Some(transfer) = &mut self.current.transfer {
                transfer.started += elapsed;
                transfer.updated += elapsed;
            }
        }
        Ok(())
    }

    fn next_block(&mut self, maximum: u64, traffic: CopyTraffic) -> io::Result<(u64, bool)> {
        let cancel = self.cancel.clone();
        let Some(transfer) = &cancel.control.transfer else {
            self.check_cancelled()?;
            return Ok((maximum, false));
        };
        loop {
            self.checkpoint()?;
            let state = transfer.state.lock().unwrap();
            cancel.check_requested()?;
            if state.pause != PauseState::Running {
                continue;
            }
            let rate = match traffic {
                CopyTraffic::Payload => state.copy_rate,
                CopyTraffic::Readback => state.readback_rate,
            };
            match self.pacer.block(maximum, rate, Instant::now()) {
                Ok(bytes) => return Ok((bytes, rate != 0)),
                Err(wait) => {
                    let _state = transfer.changed.wait_timeout(state, wait).unwrap();
                }
            }
        }
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
            self.current.verification = None;
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
        if let Some(view) = &self.view
            && let Ok(mut snapshot) = view.lock()
        {
            *snapshot = self.current.clone();
            self.last_update = Some(now);
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
                let full = buffer.len() as u64;
                let maximum =
                    expected.map_or(full, |size| size.saturating_sub(written).clamp(1, full));
                let (limit, paced) = progress.next_block(maximum, CopyTraffic::Payload)?;
                let read = match input.read(&mut buffer[..limit as usize]) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => result?,
                };
                progress.checkpoint()?;
                if read == 0 {
                    return Ok(written);
                }
                let mut pending = &buffer[..read];
                while !pending.is_empty() {
                    // SFTP writes can submit multiple packets yet acknowledge
                    // only part of the buffer. Pause after the whole block.
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
                // Send the admitted block before a buffered writer can combine
                // it with data from later pacing intervals.
                if paced {
                    output.flush()?;
                }
                progress.pacer.spend(read as u64);
            }
        })
    }

    /// Feed an archive from a bounded source while reporting bytes consumed.
    pub(crate) fn read_stream(
        &mut self,
        path: &Path,
        expected: u64,
        input: impl Read,
        consume: impl FnOnce(&mut dyn Read) -> io::Result<()>,
    ) -> io::Result<u64> {
        self.current.reading_inputs = true;
        self.copy_tracked(path, Some(expected), |progress| {
            let mut reader = ProgressReader {
                input: input.take(expected),
                progress,
                bytes: 0,
            };
            consume(&mut reader)?;
            reader.progress.check_cancelled()?;
            Ok(reader.bytes)
        })
    }

    pub fn copy_local<R: Read, W: Write>(
        &mut self,
        path: &Path,
        expected: u64,
        input: &mut R,
        output: &mut W,
    ) -> io::Result<u64> {
        // Keep std's file-to-file fast path available (e.g. copy_file_range on
        // Linux), while still publishing progress during large local copies.
        const CHUNK: u64 = 1024 * 1024;
        self.copy_tracked(path, Some(expected), |progress| {
            let mut written = 0;
            progress.checkpoint()?;
            while written < expected {
                let (limit, _) =
                    progress.next_block(CHUNK.min(expected - written), CopyTraffic::Payload)?;
                let chunk = io::copy(&mut (&mut *input).take(limit), output)?;
                written += chunk;
                progress.pacer.spend(chunk);
                progress.wrote(chunk);
                progress.checkpoint()?;
                if chunk < limit {
                    return Ok(written);
                }
            }
            // The caller checks handle/path metadata after reading. Stopping
            // at the captured length also bounds a continuously growing file.
            Ok(written)
        })
    }

    /// Independently read back an unpublished regular file. The expected length
    /// bounds work even if it grows; one extra byte detects an appended tail.
    /// Verification reads never count as destination writes.
    pub fn verify_copy(
        &mut self,
        path: &Path,
        target: &Location,
        expected: u64,
        source_hash: [u8; 32],
        input: &mut impl Read,
    ) -> io::Result<()> {
        let actual = self.hash_file_with(path, expected, input, true)?;
        verify_hash(target, expected, source_hash, actual)
    }

    /// The published target is being checked while the source is quarantined.
    pub(crate) fn verify_copy_before_source_removal(
        &mut self,
        path: &Path,
        target: &Location,
        expected: u64,
        source_hash: [u8; 32],
        input: &mut impl Read,
    ) -> io::Result<()> {
        let actual = self.hash_file_with(path, expected, input, false)?;
        verify_hash(target, expected, source_hash, actual)
    }

    /// Hash a regular file at its captured length, with a one-byte growth probe.
    pub(crate) fn hash_file(
        &mut self,
        path: &Path,
        expected: u64,
        input: &mut impl Read,
    ) -> io::Result<Option<[u8; 32]>> {
        self.hash_file_with(path, expected, input, false)
            .map(HashRead::digest)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn hash_copy_check(
        &mut self,
        path: &Path,
        expected: u64,
        input: &mut impl Read,
    ) -> io::Result<HashRead> {
        self.hash_file_with(path, expected, input, true)
    }

    fn hash_file_with(
        &mut self,
        path: &Path,
        expected: u64,
        input: &mut impl Read,
        staged_copy: bool,
    ) -> io::Result<HashRead> {
        let check = |progress: &mut Self| {
            if staged_copy {
                progress.checkpoint()
            } else {
                progress.check_cancelled()
            }
        };
        self.current.item = Some(path.to_owned());
        self.current.waiting_for_conflict = false;
        self.current.transfer = None;
        self.current.verification = Some(Verification { read: 0, expected });
        self.publish(Instant::now(), true);
        let mut hash = Sha256::new();
        let mut read = 0_u64;
        let mut buffer = [0; 64 * 1024];
        loop {
            let limit = (expected - read).min(buffer.len() as u64) as usize;
            let size = if limit == 0 { 1 } else { limit };
            let size = if staged_copy {
                let (limit, _) = self.next_block(size as u64, CopyTraffic::Readback)?;
                limit as usize
            } else {
                check(self)?;
                size
            };
            let count = match input.read(&mut buffer[..size]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            check(self)?;
            if count == 0 {
                break;
            }
            if limit == 0 {
                return Ok(HashRead::ExtraData);
            }
            if staged_copy {
                self.pacer.spend(count as u64);
            }
            hash.update(&buffer[..count]);
            read += count as u64;
            self.current.verification.as_mut().unwrap().read = read;
            self.publish(Instant::now(), read == count as u64);
        }
        check(self)?;
        if read != expected {
            return Ok(HashRead::Short(read));
        }
        self.publish(Instant::now(), true);
        Ok(HashRead::Complete(hash.finalize().into()))
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
        self.current.verification = None;
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

struct ProgressReader<'a, R> {
    input: R,
    progress: &'a mut Progress,
    bytes: u64,
}

impl<R: Read> Read for ProgressReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.progress.check_cancelled()?;
        let count = self.input.read(buffer)?;
        self.progress.check_cancelled()?;
        self.bytes += count as u64;
        self.progress.wrote(count as u64);
        Ok(count)
    }
}

fn verify_hash(
    target: &Location,
    expected: u64,
    source_hash: [u8; 32],
    actual: HashRead,
) -> io::Result<()> {
    let error = match actual {
        HashRead::Complete(digest) if digest == source_hash => return Ok(()),
        HashRead::Complete(_) => CopyVerificationError::DigestMismatch(target.clone()),
        HashRead::Short(actual) => CopyVerificationError::CopySize {
            target: target.clone(),
            expected,
            actual,
        },
        HashRead::ExtraData => CopyVerificationError::ExtraData {
            target: target.clone(),
            expected,
        },
    };
    Err(error.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contextual_io_failure_retains_the_original_error() {
        let source = io::Error::from(io::ErrorKind::PermissionDenied);
        let error = TransferError::io(source, |source| format!("fixture.txt: {source}"));
        let original = std::error::Error::source(&error)
            .and_then(|cause| cause.downcast_ref::<io::Error>())
            .unwrap();
        assert_eq!(original.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().starts_with("fixture.txt: "));
    }

    #[test]
    fn transfer_speed_flushes_paced_output_to_the_staged_writer() {
        let (mut progress, view) = Progress::tracked_transfer();
        let mut limits = TransferLimits::default();
        limits.global_kib = 64;
        view.set_transfer_limits(&limits);
        let payload = b"paced payload";
        let mut output = io::BufWriter::new(Vec::new());
        assert_eq!(
            progress
                .copy(
                    Path::new("source"),
                    Some(payload.len() as u64),
                    &mut payload.as_slice(),
                    &mut output,
                )
                .unwrap(),
            payload.len() as u64
        );
        assert_eq!(output.get_ref().as_slice(), payload);
    }

    #[test]
    fn copy_hash_verification_checks_content_length_errors_and_write_accounting() {
        use std::io::Cursor;
        let known_abc = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        let path = Path::new("source");
        let target = Location::Local("destination".into());
        let (mut progress, mut view) = Progress::tracked();
        let mut source = CopyReader::new(Cursor::new(b"abc"), true);
        let mut destination = Vec::new();
        progress
            .copy(path, Some(3), &mut source, &mut destination)
            .unwrap();
        assert_eq!(source.finish(), Some(known_abc));
        progress
            .verify_copy(path, &target, 3, known_abc, &mut Cursor::new(&destination))
            .unwrap();
        let snapshot = view.snapshot();
        assert_eq!(snapshot.total_written, 3);
        assert!(snapshot.transfer.is_none());
        assert_eq!(snapshot.verification.as_ref().unwrap().read, 3);
        for (changed, expected) in [
            (
                b"abd".as_slice(),
                CopyVerificationError::DigestMismatch(target.clone()),
            ),
            (
                b"ab",
                CopyVerificationError::CopySize {
                    target: target.clone(),
                    expected: 3,
                    actual: 2,
                },
            ),
            (
                b"abcd",
                CopyVerificationError::ExtraData {
                    target: target.clone(),
                    expected: 3,
                },
            ),
        ] {
            let error = progress
                .verify_copy(path, &target, 3, known_abc, &mut Cursor::new(changed))
                .unwrap_err();
            assert!(
                matches!(TransferError::from(error), TransferError::Failed(Failure::CopyVerification(reason)) if *reason == expected)
            );
        }
        let after_publication = progress
            .verify_copy_before_source_removal(
                path,
                &target,
                3,
                known_abc,
                &mut Cursor::new(b"abd"),
            )
            .unwrap_err();
        assert!(
            matches!(TransferError::from(after_publication), TransferError::Failed(Failure::CopyVerification(reason))
            if *reason == CopyVerificationError::DigestMismatch(target.clone()))
        );
        progress
            .verify_copy(
                path,
                &target,
                0,
                Sha256::digest([]).into(),
                &mut Cursor::new([]),
            )
            .unwrap();
        struct FailedRead;
        impl Read for FailedRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::PermissionDenied.into())
            }
        }
        assert_eq!(
            progress
                .verify_copy(path, &target, 3, known_abc, &mut FailedRead)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn copy_hash_verification_observes_stop_after_reads_without_an_observer() {
        struct StopOnRead(Cancellation, usize);
        impl Read for StopOnRead {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.1 += 1;
                self.0.request();
                buffer[0] = 1;
                Ok(1)
            }
        }
        let mut progress = Progress::default();
        let mut input = StopOnRead(progress.cancellation(), 0);
        let error = progress
            .verify_copy(
                Path::new("source"),
                &Location::Local("destination".into()),
                1024,
                [0; 32],
                &mut input,
            )
            .unwrap_err();
        assert!(matches!(
            TransferError::from(error),
            TransferError::Cancelled
        ));
        assert_eq!(input.1, 1);
    }

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
