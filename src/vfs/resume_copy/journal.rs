//! Durable receipts for one worker and one staged copy.
use super::{HeldSource, Intent, Original, Receipt, RenamedIdentity, SavedTime};
use crate::recovery::{same_owned_marker, sync_directory};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const PREFIX: &str = "copy-";
const SUFFIX: &str = ".json";
const VERSION: u32 = 1;
const MOVE_VERSION: u32 = 2;
const MOVE_ACCESS_TIMES_VERSION: u32 = 3;
const MAX_BYTES: u64 = 128 * 1024;
// Holds original dates for the existing 100,000-entry move limit. Recovery
// listing skips this section and reads the dates only when finishing a move.
const MAX_ACCESS_TIMES_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 1024;

pub(super) struct Ticket {
    file: File,
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

fn invalid_access_times() -> io::Error {
    io::Error::other(crate::i18n::text!(
        "Некорректная запись времени доступа для переноса",
    ))
}

#[derive(Deserialize, Serialize)]
pub(super) enum Event {
    Publication(RenamedIdentity),
    Removing(HeldSource),
    Removed,
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
    ) -> io::Result<Self> {
        let encoded = folder_access_times
            .map(serde_json::to_vec)
            .transpose()
            .map_err(io::Error::other)?;
        receipt.folder_access_times = encoded.as_ref().map(|bytes| AccessTimesSection {
            bytes: bytes.len() as u64,
            digest: Sha256::digest(bytes).into(),
        });
        if let Some(section) = &receipt.folder_access_times
            && !valid_access_times(&receipt, section)
        {
            return Err(invalid_access_times());
        }
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        let mut file = tempfile::Builder::new()
            .prefix(PREFIX)
            .suffix(".pending")
            .tempfile_in(directory)?;
        file.as_file().try_lock().map_err(io::Error::other)?;
        let bytes = serde_json::to_vec(&(version(&receipt), &receipt)).map_err(io::Error::other)?;
        if bytes.len() as u64 + u64::from(encoded.is_some()) > MAX_BYTES {
            return Err(io::Error::other("Copy recovery record exceeds 128 KiB"));
        }
        file.write_all(&bytes)?;
        let access_times_offset = if let Some(encoded) = encoded {
            file.write_all(b"\n")?;
            file.write_all(&encoded)?;
            Some(bytes.len() as u64 + 1)
        } else {
            None
        };
        file.as_file().sync_all()?;
        let path = file.path().with_extension("json");
        let file = file.persist_noclobber(&path).map_err(|e| e.error)?;
        sync_directory(directory)?;
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

    pub(super) fn open(path: PathBuf) -> io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        if !file.metadata()?.is_file() || !same_owned_marker(&file, &path)? {
            return Err(io::Error::other("Copy recovery record identity changed"));
        }
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(error) => return Err(io::Error::other(error)),
        }
        let mut reader = BufReader::new(&file);
        let mut header = Vec::new();
        (&mut reader)
            .take(MAX_BYTES + 1)
            .read_until(b'\n', &mut header)?;
        if header.len() as u64 > MAX_BYTES {
            return Err(io::Error::other(
                "Invalid copy recovery record size or identity",
            ));
        }
        let (stored_version, receipt): (u32, Receipt) =
            serde_json::from_slice(&header).map_err(io::Error::other)?;
        if stored_version != version(&receipt) {
            return Err(io::Error::other("Unsupported copy recovery record version"));
        }
        // New folder moves put the length-delimited date table between the
        // first JSON line and the events. Old receipts go straight to events.
        let has_separator = header.ends_with(b"\n");
        let access_times_offset = if let Some(section) = &receipt.folder_access_times {
            let offset = header.len() as u64;
            if !has_separator
                || !valid_access_times(&receipt, section)
                || offset + section.bytes > file.metadata()?.len()
            {
                return Err(invalid_access_times());
            }
            reader.seek(SeekFrom::Start(offset + section.bytes))?;
            Some(offset)
        } else {
            None
        };
        let event_limit =
            MAX_BYTES - header.len() as u64 + u64::from(access_times_offset.is_some());
        let mut tail = Vec::new();
        reader.take(event_limit + 1).read_to_end(&mut tail)?;
        if tail.len() as u64 > event_limit || !same_owned_marker(&file, &path)? {
            return Err(io::Error::other(
                "Invalid copy recovery record size or identity",
            ));
        }
        let events = if access_times_offset.is_some() {
            if tail.is_empty() {
                None
            } else {
                Some(tail.strip_prefix(b"\n").ok_or_else(invalid_access_times)?)
            }
        } else {
            has_separator.then_some(tail.as_slice())
        };
        let mut publication = None;
        let mut holding = None;
        let mut removed = false;
        for line in events
            .into_iter()
            .flat_map(|bytes| bytes.split(|&byte| byte == b'\n'))
        {
            let event = serde_json::from_slice(line).map_err(io::Error::other)?;
            match event {
                Event::Publication(seal) if receipt.intent == Intent::Move && holding.is_none() => {
                    publication = Some(seal);
                }
                Event::Removing(location) if publication.is_some() && !removed => {
                    holding = Some(location);
                }
                Event::Removed if holding.is_some() && !removed => removed = true,
                _ => return Err(io::Error::other("Invalid move recovery record sequence")),
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

    pub(super) fn folder_access_times(&self) -> io::Result<Option<Vec<Option<SystemTime>>>> {
        let Some(offset) = self.access_times_offset else {
            return Ok(None);
        };
        let section = self
            .receipt
            .folder_access_times
            .as_ref()
            .ok_or_else(invalid_access_times)?;
        let mut bytes = vec![0; section.bytes as usize];
        let mut file = &self.file;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut bytes)?;
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if digest != section.digest || !same_owned_marker(file, &self.path)? {
            return Err(invalid_access_times());
        }
        let saved: Vec<Option<SavedTime>> =
            serde_json::from_slice(&bytes).map_err(io::Error::other)?;
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

    pub(super) fn seal(&self, proof: RenamedIdentity) -> io::Result<()> {
        if self.publication() != Some(proof) {
            self.record(&Event::Publication(proof))?;
            self.publication.set(Some(proof));
        }
        Ok(())
    }

    pub(super) fn record(&self, event: &Event) -> io::Result<()> {
        if self.receipt.intent != Intent::Move || !same_owned_marker(&self.file, &self.path)? {
            return Err(io::Error::other("Move recovery record identity changed"));
        }
        let mut bytes = vec![b'\n'];
        serde_json::to_writer(&mut bytes, event).map_err(io::Error::other)?;
        let mut file = &self.file;
        let access_times_bytes = self
            .receipt
            .folder_access_times
            .as_ref()
            .map_or(0, |section| section.bytes + 1);
        if file.seek(SeekFrom::End(0))? + bytes.len() as u64 > MAX_BYTES + access_times_bytes {
            return Err(io::Error::other("Move recovery record exceeds 128 KiB"));
        }
        file.write_all(&bytes)?;
        file.sync_all()
    }

    pub(super) fn finish(&self) -> io::Result<()> {
        if !same_owned_marker(&self.file, &self.path)? {
            return Err(io::Error::other("Copy recovery record identity changed"));
        }
        fs::remove_file(&self.path)?;
        sync_directory(self.path.parent().unwrap())
    }
}

fn valid_access_times(receipt: &Receipt, section: &AccessTimesSection) -> bool {
    receipt.intent == Intent::Move
        && matches!(receipt.original, Original::Folder(_))
        && (1..=MAX_ACCESS_TIMES_BYTES).contains(&section.bytes)
}

pub(super) fn valid_name(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|text| text.strip_prefix(PREFIX))
        .and_then(|text| text.strip_suffix(SUFFIX))
        .is_some_and(|token| !token.is_empty() && token.bytes().all(|b| b.is_ascii_alphanumeric()))
}

pub(super) fn visit_available(
    directory: &Path,
    mut visit: impl FnMut(Ticket) -> Result<(), String>,
) -> Vec<String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return vec![],
        Err(error) => return vec![error.to_string()],
    };
    let mut errors = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index == MAX_ENTRIES {
            errors.push("Copy recovery scan stopped after 1024 entries".into());
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
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => errors.push(error.to_string()),
            },
            Ok(_) => {}
            Err(error) => errors.push(error.to_string()),
        }
    }
    errors
}
