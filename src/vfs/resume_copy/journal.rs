//! Durable receipts for one worker and one staged copy.
use super::{HeldSource, Intent, Original, Paths, Receipt, RenamedIdentity, SavedTime};
use crate::file_error::{
    RecoveryRecordAction as Action, RecoveryRecordError, RecoveryRecordProblem,
    RecoveryRecordSection, ResumeRecordEvent,
};
use crate::recovery::{
    check_record_identity, record_name_matches, remove_record, same_owned_marker, store_record,
    LockedRecord,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const PREFIX: &str = "copy-";
const VERSION: u32 = 1;
const MOVE_VERSION: u32 = 2;
const MOVE_ACCESS_TIMES_VERSION: u32 = 3;
const MAX_BYTES: u64 = 128 * 1024;
// Holds original dates for the existing 100,000-entry move limit. Recovery
// listing skips this section and reads the dates only when finishing a move.
const MAX_ACCESS_TIMES_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 1024;

pub(super) struct Ticket {
    file: LockedRecord,
    path: PathBuf,
    pub(super) receipt: Receipt,
    publication: Cell<Option<RenamedIdentity>>,
    pub(super) holding: Option<HeldSource>,
    pub(super) removed: bool,
    access_times_offset: Option<u64>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct AccessTimesSection {
    bytes: u64,
    digest: [u8; 32],
}

#[derive(Deserialize, Serialize)]
pub(super) enum Event {
    Publication(RenamedIdentity),
    Removing(HeldSource),
    Removed,
}

impl Event {
    fn kind(&self) -> ResumeRecordEvent {
        match self {
            Self::Publication(_) => ResumeRecordEvent::Publication,
            Self::Removing(_) => ResumeRecordEvent::HoldingSource,
            Self::Removed => ResumeRecordEvent::SourceRemoved,
        }
    }
}

fn version(receipt: &Receipt) -> u32 {
    match receipt.intent {
        Intent::Copy => VERSION,
        Intent::Move if receipt.folder_access_times.is_some() => MOVE_ACCESS_TIMES_VERSION,
        Intent::Move => MOVE_VERSION,
    }
}

impl Ticket {
    pub(super) fn create(
        directory: &Path,
        mut receipt: Receipt,
        folder_access_times: Option<&[Option<SavedTime>]>,
    ) -> Result<Self, RecoveryRecordError> {
        let encoded = folder_access_times
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|error| RecoveryRecordError::encoding(directory, error))?;
        receipt.folder_access_times = encoded.as_ref().map(|bytes| AccessTimesSection {
            bytes: bytes.len() as u64,
            digest: Sha256::digest(bytes).into(),
        });
        if let Some(section) = &receipt.folder_access_times
            && !valid_access_times(&receipt, section)
        {
            return Err(RecoveryRecordError::new(
                directory,
                RecoveryRecordProblem::InvalidAccessTimes,
            ));
        }
        let bytes = serde_json::to_vec(&(version(&receipt), &receipt))
            .map_err(|error| RecoveryRecordError::encoding(directory, error))?;
        if bytes.len() as u64 + u64::from(encoded.is_some()) > MAX_BYTES {
            return Err(RecoveryRecordError::new(
                directory,
                RecoveryRecordProblem::SizeLimit(MAX_BYTES),
            ));
        }
        let access_times_offset = encoded.as_ref().map(|_| bytes.len() as u64 + 1);
        let (file, path) = if let Some(encoded) = encoded {
            store_record(directory, PREFIX, &[&bytes, b"\n", &encoded])?
        } else {
            store_record(directory, PREFIX, &[&bytes])?
        };
        Ok(Self {
            file,
            path,
            receipt,
            publication: Cell::new(None),
            holding: None,
            removed: false,
            access_times_offset,
        })
    }

    pub(super) fn open(path: PathBuf) -> Result<Option<Self>, RecoveryRecordError> {
        let failure = |problem| RecoveryRecordError::new(&path, problem);
        let read = |error| failure(RecoveryRecordProblem::Read(error));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(read)?;
        if !file.metadata().map_err(read)?.is_file()
            || !same_owned_marker(&file, &path).map_err(read)?
        {
            return Err(failure(RecoveryRecordProblem::IdentityChanged));
        }
        let file = match LockedRecord::try_lock(file) {
            Ok(file) => file,
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(error)) => {
                return Err(failure(RecoveryRecordProblem::Lock(error)))
            }
        };
        let mut reader = BufReader::new(file.as_file());
        let mut header = Vec::new();
        (&mut reader)
            .take(MAX_BYTES + 1)
            .read_until(b'\n', &mut header)
            .map_err(read)?;
        if header.len() as u64 > MAX_BYTES {
            return Err(failure(RecoveryRecordProblem::SizeLimit(MAX_BYTES)));
        }
        let invalid_header = |source| {
            failure(RecoveryRecordProblem::Json {
                section: RecoveryRecordSection::Header,
                source,
            })
        };
        // Read the envelope first so an unfamiliar receipt schema still reports its version.
        let (stored, _): (u32, serde::de::IgnoredAny) =
            serde_json::from_slice(&header).map_err(invalid_header)?;
        if !matches!(stored, VERSION | MOVE_VERSION | MOVE_ACCESS_TIMES_VERSION) {
            return Err(failure(RecoveryRecordProblem::UnsupportedVersion(stored)));
        }
        let (_, receipt): (u32, Receipt) =
            serde_json::from_slice(&header).map_err(invalid_header)?;
        let expected = version(&receipt);
        if stored != expected {
            return Err(failure(RecoveryRecordProblem::VersionMismatch {
                stored,
                expected,
            }));
        }
        // New folder moves put the length-delimited date table between the
        // first JSON line and the events. Old receipts go straight to events.
        let has_separator = header.ends_with(b"\n");
        let access_times_offset = if let Some(section) = &receipt.folder_access_times {
            let offset = header.len() as u64;
            if !has_separator
                || !valid_access_times(&receipt, section)
                || offset + section.bytes > file.as_file().metadata().map_err(read)?.len()
            {
                return Err(failure(RecoveryRecordProblem::InvalidAccessTimes));
            }
            reader
                .seek(SeekFrom::Start(offset + section.bytes))
                .map_err(read)?;
            Some(offset)
        } else {
            None
        };
        let event_limit =
            MAX_BYTES - header.len() as u64 + u64::from(access_times_offset.is_some());
        let mut tail = Vec::new();
        reader
            .take(event_limit + 1)
            .read_to_end(&mut tail)
            .map_err(read)?;
        if tail.len() as u64 > event_limit {
            return Err(failure(RecoveryRecordProblem::SizeLimit(MAX_BYTES)));
        }
        if !same_owned_marker(file.as_file(), &path).map_err(read)? {
            return Err(failure(RecoveryRecordProblem::IdentityChanged));
        }
        let events = if access_times_offset.is_some() {
            if tail.is_empty() {
                None
            } else {
                Some(
                    tail.strip_prefix(b"\n")
                        .ok_or_else(|| failure(RecoveryRecordProblem::InvalidAccessTimes))?,
                )
            }
        } else {
            has_separator.then_some(tail.as_slice())
        };
        let mut publication = None;
        let mut holding = None;
        let mut removed = false;
        for (index, line) in events
            .into_iter()
            .flat_map(|bytes| bytes.split(|&byte| byte == b'\n'))
            .enumerate()
        {
            let number = index + 1;
            let event = serde_json::from_slice(line).map_err(|source| {
                failure(RecoveryRecordProblem::Json {
                    section: RecoveryRecordSection::Event(number),
                    source,
                })
            })?;
            match event {
                Event::Publication(seal) if receipt.intent == Intent::Move && holding.is_none() => {
                    publication = Some(seal);
                }
                Event::Removing(location) if publication.is_some() && !removed => {
                    holding = Some(location);
                }
                Event::Removed if holding.is_some() && !removed => removed = true,
                _ => return Err(failure(RecoveryRecordProblem::EventOrder(number))),
            }
        }
        Ok(Some(Self {
            file,
            path,
            receipt,
            publication: Cell::new(publication),
            holding,
            removed,
            access_times_offset,
        }))
    }

    pub(super) fn paths(&self) -> Result<Paths, RecoveryRecordError> {
        self.receipt
            .paths()
            .ok_or_else(|| self.error(RecoveryRecordProblem::InvalidPaths))
    }

    pub(super) fn error(&self, problem: RecoveryRecordProblem) -> RecoveryRecordError {
        RecoveryRecordError::new(&self.path, problem)
    }

    pub(super) fn folder_access_times(
        &self,
    ) -> Result<Option<Vec<Option<SystemTime>>>, RecoveryRecordError> {
        let Some(offset) = self.access_times_offset else {
            return Ok(None);
        };
        let failure = |problem| RecoveryRecordError::new(&self.path, problem);
        let read = |error| failure(RecoveryRecordProblem::Read(error));
        let section = self
            .receipt
            .folder_access_times
            .as_ref()
            .ok_or_else(|| failure(RecoveryRecordProblem::InvalidAccessTimes))?;
        let mut bytes = vec![0; section.bytes as usize];
        let mut file = self.file.as_file();
        file.seek(SeekFrom::Start(offset)).map_err(read)?;
        file.read_exact(&mut bytes).map_err(read)?;
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if !same_owned_marker(file, &self.path).map_err(read)? {
            return Err(failure(RecoveryRecordProblem::IdentityChanged));
        }
        if digest != section.digest {
            return Err(failure(RecoveryRecordProblem::InvalidAccessTimes));
        }
        let saved: Vec<Option<SavedTime>> = serde_json::from_slice(&bytes).map_err(|source| {
            failure(RecoveryRecordProblem::Json {
                section: RecoveryRecordSection::AccessTimes,
                source,
            })
        })?;
        Ok(Some(
            saved
                .iter()
                .map(|time| time.as_ref().and_then(SavedTime::get))
                .collect(),
        ))
    }

    pub(super) fn name(&self) -> &str {
        // The path is generated by create or checked before open.
        self.path.file_name().and_then(OsStr::to_str).unwrap()
    }

    pub(super) fn publication(&self) -> Option<RenamedIdentity> {
        self.publication.get()
    }

    pub(super) fn seal(&self, proof: RenamedIdentity) -> Result<(), RecoveryRecordError> {
        if self.publication() != Some(proof) {
            self.record(&Event::Publication(proof))?;
            self.publication.set(Some(proof));
        }
        Ok(())
    }

    pub(super) fn record(&self, event: &Event) -> Result<(), RecoveryRecordError> {
        if self.receipt.intent != Intent::Move {
            return Err(self.error(RecoveryRecordProblem::InvalidProof));
        }
        check_record_identity(self.file.as_file(), &self.path)?;
        let kind = event.kind();
        let mut bytes = vec![b'\n'];
        serde_json::to_writer(&mut bytes, event).map_err(|source| {
            self.error(RecoveryRecordProblem::Encode {
                event: Some(kind),
                source,
            })
        })?;
        let mut file = self.file.as_file();
        let access_times_bytes = self
            .receipt
            .folder_access_times
            .as_ref()
            .map_or(0, |section| section.bytes + 1);
        let start = file
            .seek(SeekFrom::End(0))
            .map_err(|error| RecoveryRecordError::io(&self.path, Action::SeekEnd, error))?;
        if start.saturating_add(bytes.len() as u64) > MAX_BYTES + access_times_bytes {
            return Err(self.error(RecoveryRecordProblem::SizeLimit(MAX_BYTES)));
        }
        if let Err(source) = file.write_all(&bytes) {
            // The ticket remains locked. Remove only bytes from this failed append.
            if let Err(restore) = restore_append(file, start) {
                return Err(self.error(RecoveryRecordProblem::AppendRollback {
                    event: kind,
                    source,
                    restore,
                }));
            }
            return Err(RecoveryRecordError::io(
                &self.path,
                Action::Append(kind),
                source,
            ));
        }
        // A complete event may already be readable after a synchronization error.
        file.sync_all()
            .map_err(|error| RecoveryRecordError::io(&self.path, Action::SyncEvent(kind), error))
    }

    pub(super) fn finish(&self) -> Result<(), RecoveryRecordError> {
        remove_record(self.file.as_file(), &self.path)
    }
}

fn restore_append(file: &File, start: u64) -> io::Result<()> {
    if file.metadata()?.len() > start {
        file.set_len(start)?;
        file.sync_all()?;
    }
    Ok(())
}

fn valid_access_times(receipt: &Receipt, section: &AccessTimesSection) -> bool {
    receipt.intent == Intent::Move
        && matches!(receipt.original, Original::Folder(_))
        && (1..=MAX_ACCESS_TIMES_BYTES).contains(&section.bytes)
}

pub(super) fn valid_name(name: &OsStr) -> bool {
    record_name_matches(name, PREFIX)
}

pub(super) fn visit_available(
    directory: &Path,
    mut visit: impl FnMut(Ticket) -> Result<(), RecoveryRecordError>,
) -> Vec<RecoveryRecordError> {
    let read =
        |error| RecoveryRecordError::new(directory, RecoveryRecordProblem::ReadDirectory(error));
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return vec![],
        Err(error) => return vec![read(error)],
    };
    let mut errors = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index == MAX_ENTRIES {
            errors.push(RecoveryRecordError::new(
                directory,
                RecoveryRecordProblem::ScanLimit(MAX_ENTRIES),
            ));
            break;
        }
        match entry {
            Ok(entry) if valid_name(&entry.file_name()) => match Ticket::open(entry.path()) {
                Ok(Some(ticket)) => {
                    if let Err(error) = visit(ticket) {
                        errors.push(error);
                    }
                }
                Ok(None) => {}
                Err(error) if error.is_missing() => {}
                Err(error) => errors.push(error),
            },
            Ok(_) => {}
            Err(error) => errors.push(read(error)),
        }
    }
    errors
}
