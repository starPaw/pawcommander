//! Explicit recovery of checked file and folder copies, including copy-based moves.
mod journal;
#[cfg(test)]
pub(super) mod tests;
mod tree;

use super::{
    copy_metadata,
    cross_move::{self, MoveSource, Phase},
    read_scope::ReadScope,
    sftp::remote,
    staged_file::StagedFile,
    transfer::{
        check_copy_space, entry_metadata, publish_checked_replacement, rename_for_publication,
        with_preserved_staging, EntryMetadata, TransferItem, TransferSourceManifest,
    },
    Location,
};
use crate::app::SavedLocation;
use crate::file_error::{
    AccessAction, AccessError, Failure, MutationError, PublicationError, RecoveryNotice,
    RecoveryRecordError, RecoveryRecordProblem, ResumeCheckProblem, ResumeRole, StagingError,
};
use crate::i18n::text;
use crate::operations::{AccessStream, Cancellation, Progress, TransferError};
use crate::result_history::Outcome;
use journal::{Event, Ticket};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
enum FileStamp {
    Local {
        device: u64,
        inode: u64,
        size: u64,
        modified: (i64, i64),
        changed: (i64, i64),
        mode: u32,
    },
    Sftp {
        size: u64,
        modified: u64,
        mode: u32,
        uid: Option<u32>,
        gid: Option<u32>,
    },
}

impl FileStamp {
    fn from(
        file: &Location,
        metadata: &EntryMetadata,
        role: ResumeRole,
    ) -> Result<Self, TransferError> {
        let incomplete = || role.error(file, ResumeCheckProblem::IncompleteMetadata);
        match metadata {
            EntryMetadata::Local(file) if file.is_file() => Ok(Self::Local {
                device: file.dev(),
                inode: file.ino(),
                size: file.len(),
                modified: (file.mtime(), file.mtime_nsec()),
                changed: (file.ctime(), file.ctime_nsec()),
                mode: file.mode(),
            }),
            EntryMetadata::Remote(stat) if stat.perm.is_none() => Err(incomplete()),
            EntryMetadata::Remote(stat) if stat.is_file() => Ok(Self::Sftp {
                size: stat.size.ok_or_else(incomplete)?,
                modified: stat.mtime.ok_or_else(incomplete)?,
                mode: stat.perm.ok_or_else(incomplete)?,
                uid: stat.uid,
                gid: stat.gid,
            }),
            _ => Err(role.error(file, ResumeCheckProblem::WrongType)),
        }
    }

    fn size(self) -> u64 {
        match self {
            Self::Local { size, .. } | Self::Sftp { size, .. } => size,
        }
    }

    fn ignore_rename_time(mut self) -> Self {
        if let Self::Local { changed, .. } = &mut self {
            *changed = (0, 0);
        }
        self
    }
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct FileProof {
    stamp: FileStamp,
    digest: [u8; 32],
}

#[derive(Deserialize, Serialize)]
struct SavedTime {
    before_epoch: bool,
    distance: Duration,
}

impl From<SystemTime> for SavedTime {
    fn from(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(distance) => Self {
                before_epoch: false,
                distance,
            },
            Err(error) => Self {
                before_epoch: true,
                distance: error.duration(),
            },
        }
    }
}

impl SavedTime {
    fn get(&self) -> Option<SystemTime> {
        if self.before_epoch {
            UNIX_EPOCH.checked_sub(self.distance)
        } else {
            UNIX_EPOCH.checked_add(self.distance)
        }
    }
}

impl FileProof {
    fn read(
        file: &Location,
        role: ResumeRole,
        progress: &mut Progress,
    ) -> Result<(Self, EntryMetadata), TransferError> {
        let cancel = progress.cancellation().pausable_scan();
        let (scope, name) = file_scope(file, role)?;
        let metadata = required_metadata(file, role)?;
        let stamp = FileStamp::from(file, &metadata, role)?;
        progress.set_copy_endpoints(file, file);
        let digest = scope
            .read_checked(name, &cancel, |source, size, input| {
                progress
                    .hash_copy_check(source.path(), size, &mut AccessStream::new(input, source))?
                    .ok_or_else(|| role.error(source, ResumeCheckProblem::Changed))
            })?
            .ok_or_else(|| role.error(file, ResumeCheckProblem::Missing))?;
        verify_stamp(file, stamp, role)?;
        Ok((Self { stamp, digest }, metadata))
    }
}

#[derive(Deserialize, Serialize)]
struct Receipt {
    #[serde(default, skip_serializing_if = "Intent::is_copy")]
    intent: Intent,
    source: SavedLocation,
    target: SavedLocation,
    stage_name: String,
    stage_identity: Option<(u64, u64)>,
    original: Original,
    source_accessed: Option<SavedTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    folder_access_times: Option<journal::AccessTimesSection>,
    previous: Option<FileProof>,
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
enum Intent {
    #[default]
    Copy,
    Move,
}

impl Intent {
    fn is_copy(&self) -> bool {
        *self == Self::Copy
    }

    fn metadata(self) -> copy_metadata::Mode {
        match self {
            Self::Copy => copy_metadata::Mode::Final,
            Self::Move => copy_metadata::Mode::KeepReadableForMove,
        }
    }
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
enum RenamedIdentity {
    File(FileStamp),
    Folder([u8; 32]),
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum HeldSource {
    Verified(SourceStage),
    Legacy(SavedLocation),
}

#[derive(Deserialize, Serialize)]
struct SourceStage {
    stage_name: String,
    stage_identity: Option<(u64, u64)>,
    source_identity: RenamedIdentity,
}

// Keep the version-one file record readable, including copies interrupted by an
// earlier build. Folder receipts add a different proof under the same field.
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum Original {
    File(FileProof),
    Folder(tree::Proof),
}

struct Paths {
    source: Location,
    target: Location,
    stage: Location,
}

impl Receipt {
    fn paths(&self) -> Option<Paths> {
        let source = self.source.clone().into_location()?;
        let target = self.target.clone().into_location()?;
        if source == target
            || source.is_archive()
            || target.is_archive()
            || !source.path().is_absolute()
            || !target.path().is_absolute()
        {
            return None;
        }
        let stage = staging_at(&target, &self.stage_name)?;
        Some(Paths {
            source,
            target,
            stage,
        })
    }
}

fn staging_at(item: &Location, stage_name: &str) -> Option<Location> {
    let prefix = if item.remote() {
        ".pawcommander-sftp-"
    } else {
        ".pawcommander-transfer-"
    };
    let valid = stage_name.strip_prefix(prefix).is_some_and(|nonce| {
        !nonce.is_empty()
            && nonce
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-')
    });
    if !valid {
        return None;
    }
    Some(item.parent()?.join(stage_name))
}

pub(crate) struct Offer {
    pub id: String,
    pub source: String,
    pub target: String,
    pub moving: bool,
}

fn receipts(directory: &Path) -> PathBuf {
    directory.join("resumable")
}

pub(crate) fn scan(directory: &Path) -> (Vec<Offer>, Vec<RecoveryRecordError>) {
    let mut offers = Vec::new();
    let errors = journal::visit_available(&receipts(directory), |ticket| {
        let paths = ticket.paths()?;
        offers.push(Offer {
            id: ticket.name().into(),
            source: paths.source.display(),
            target: paths.target.display(),
            moving: ticket.receipt.intent == Intent::Move,
        });
        Ok(())
    });
    offers.sort_by(|a, b| a.target.cmp(&b.target).then_with(|| a.id.cmp(&b.id)));
    (offers, errors)
}

pub(super) fn copy(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    directory: &Path,
    progress: &mut Progress,
    published: impl FnOnce(),
) -> Result<(), TransferError> {
    start_file(
        item,
        expected,
        directory,
        Intent::Copy,
        progress,
        published,
        |_, _, _| Ok(()),
    )
}

fn start_file(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    directory: &Path,
    intent: Intent,
    progress: &mut Progress,
    published: impl FnOnce(),
    after: impl FnOnce(&Ticket, &Paths, &mut Progress) -> Result<(), TransferError>,
) -> Result<(), TransferError> {
    let cancel = progress.cancellation();
    let source = resolved_file(&item.source, ResumeRole::Source)?;
    let target = resolved_file(&item.target, ResumeRole::Destination)?;
    let (original, source_metadata) = FileProof::read(&source, ResumeRole::Source, progress)?;
    item.verify_approved_source(Some(&source_metadata))?;
    let previous = match expected {
        Some(expected) => {
            let (proof, metadata) = FileProof::read(&target, ResumeRole::Destination, progress)?;
            if !expected.matches_file(&metadata) {
                return Err(ResumeRole::Destination.error(&target, ResumeCheckProblem::Changed));
            }
            Some(proof)
        }
        None => {
            verify_target(None, &target, false, progress)?;
            None
        }
    };
    with_preserved_staging(&target, &cancel, |stage, preserve| {
        *preserve = false;
        check_copy_space(original.stamp.size(), &target, stage)?;
        let payload = stage.join("payload");
        StagedFile::open_for(&payload, &target, cancel.clone())?.close()?;
        let receipt = Receipt {
            intent,
            source: (&source).into(),
            target: (&target).into(),
            stage_name: stage.name(),
            stage_identity: stage_identity(stage)?,
            original: Original::File(original),
            source_accessed: copy_metadata::access_time(&source_metadata).map(SavedTime::from),
            folder_access_times: None,
            previous,
        };
        let ticket = register_copy(directory, stage, receipt, None, &cancel)?;
        *preserve = true;
        let paths = ticket.paths()?;
        finish_copy(&ticket, &paths, progress, false, published)?;
        after(&ticket, &paths, progress)
    })
}

fn register_copy(
    directory: &Path,
    stage: &Location,
    receipt: Receipt,
    folder_access_times: Option<&[Option<SavedTime>]>,
    cancel: &Cancellation,
) -> Result<Ticket, TransferError> {
    let directory = receipts(directory);
    let ticket = Ticket::create(&directory, receipt, folder_access_times).map_err(|source| {
        StagingError::ResumeReceipt {
            staging: stage.clone(),
            source,
        }
    })?;
    if let Err(cause) = create_owner_marker(stage, ticket.name(), cancel) {
        if let Err(source) = ticket.finish() {
            return Err(StagingError::ResumeReceiptCleanup { cause, source }.into());
        }
        return Err(cause);
    }
    Ok(ticket)
}

fn create_owner_marker(
    stage: &Location,
    name: &str,
    cancel: &Cancellation,
) -> Result<(), TransferError> {
    StagedFile::open(&stage.join(name), cancel.clone())?.close()
}

pub(super) fn copy_folder(
    item: &TransferItem,
    directory: &Path,
    progress: &mut Progress,
    published: impl FnOnce(),
) -> Result<(), TransferError> {
    let source = resolved_file(&item.source, ResumeRole::Source)?;
    let target = resolved_file(&item.target, ResumeRole::Destination)?;
    tree::start(
        source,
        target,
        directory,
        Intent::Copy,
        progress,
        published,
        |_, _, _| Ok(()),
    )
}

pub(super) fn move_item(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    directory: &Path,
    progress: &mut Progress,
    published: impl FnOnce(),
    removed: impl FnOnce(),
    observe: &mut impl FnMut(&Location, Phase) -> Result<(), TransferError>,
) -> Result<(), TransferError> {
    let after = |ticket: &Ticket, paths: &Paths, progress: &mut Progress| {
        let canonical = TransferItem::new(
            item.original.clone(),
            paths.source.clone(),
            paths.target.clone(),
        );
        finish_move(ticket, paths, &canonical, progress, || {}, removed, observe)
    };
    if entry_metadata(&item.source)?.is_some_and(|entry| entry.is_file()) {
        start_file(
            item,
            expected,
            directory,
            Intent::Move,
            progress,
            published,
            after,
        )
    } else if expected.is_none() {
        tree::start(
            resolved_file(&item.source, ResumeRole::Source)?,
            resolved_file(&item.target, ResumeRole::Destination)?,
            directory,
            Intent::Move,
            progress,
            published,
            after,
        )
    } else {
        Err(ResumeRole::Source.error(&item.source, ResumeCheckProblem::Changed))
    }
}

fn verify_move_stage(
    ticket: &Ticket,
    paths: &Paths,
    progress: &mut Progress,
) -> Result<bool, TransferError> {
    if ticket.removed && entry_metadata(&paths.stage)?.is_none() {
        return Ok(false);
    }
    let receipt = &ticket.receipt;
    let backup = paths.stage.join("previous");
    let expected = receipt.previous.as_ref();
    let has_backup = entry_metadata(&backup)?.is_some();
    if has_backup != expected.is_some() && !(ticket.removed && !has_backup) {
        let problem = if has_backup {
            ResumeCheckProblem::Unexpected
        } else {
            ResumeCheckProblem::Missing
        };
        return Err(ResumeRole::ParkedDestination.error(&backup, problem));
    }
    let extra: &[&str] = if has_backup { &["previous"] } else { &[] };
    stage_contents(&paths.stage, receipt.stage_identity, ticket.name(), extra)?;
    if has_backup {
        verify_target(expected, &backup, true, progress)?;
    }
    Ok(true)
}

fn verify_published_move(
    ticket: &Ticket,
    paths: &Paths,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let destination = ResumeRole::Destination;
    let seal = ticket
        .publication()
        .ok_or_else(|| destination.error(&paths.target, ResumeCheckProblem::MissingProof))?;
    let check_seal = !ticket.removed;
    match &ticket.receipt.original {
        Original::File(original) => {
            let current = FileProof::read(&paths.target, destination, progress)?.0;
            if current.digest != original.digest {
                return Err(destination.error(&paths.target, ResumeCheckProblem::ContentsMismatch));
            }
            if check_seal && RenamedIdentity::File(current.stamp.ignore_rename_time()) != seal {
                return Err(destination.error(&paths.target, ResumeCheckProblem::Changed));
            }
        }
        Original::Folder(proof) => {
            tree::verify_published(paths, *proof, check_seal.then_some(seal), progress)?;
        }
    }
    Ok(())
}

fn verify_move_source(
    source: &Location,
    role: ResumeRole,
    paths: &Paths,
    original: &Original,
    renamed: Option<RenamedIdentity>,
    progress: &mut Progress,
) -> Result<RenamedIdentity, TransferError> {
    match original {
        Original::File(expected) => {
            let (actual, _) = FileProof::read(source, role, progress)?;
            let identity = RenamedIdentity::File(actual.stamp.ignore_rename_time());
            let matches = match renamed {
                Some(value) => value == identity,
                None => actual.stamp == expected.stamp,
            };
            if !matches {
                return Err(role.error(source, ResumeCheckProblem::Changed));
            }
            if actual.digest != expected.digest {
                return Err(role.error(source, ResumeCheckProblem::ContentsMismatch));
            }
            Ok(identity)
        }
        Original::Folder(proof) => {
            tree::verify_move_source(source, role, &paths.target, *proof, renamed, progress)
        }
    }
}

fn restore_held_source(
    ticket: &Ticket,
    paths: &Paths,
    progress: &mut Progress,
) -> Result<Option<RenamedIdentity>, TransferError> {
    let record = match &ticket.holding {
        Some(HeldSource::Verified(record)) => record,
        Some(HeldSource::Legacy(location)) => {
            let held = location
                .clone()
                .into_location()
                .ok_or_else(|| ticket.error(RecoveryRecordProblem::InvalidPaths))?;
            if entry_metadata(&held)?.is_some() || entry_metadata(&paths.source)?.is_none() {
                return Err(retained_source(
                    &held,
                    ResumeRole::HeldOriginal.error(&held, ResumeCheckProblem::MissingProof),
                ));
            }
            return Ok(None);
        }
        None => return Ok(None),
    };
    let stage = staging_at(&paths.source, &record.stage_name)
        .ok_or_else(|| ticket.error(RecoveryRecordProblem::InvalidPaths))?;
    let held = stage.join("source");
    let result = (|| {
        let source_present = entry_metadata(&paths.source)?.is_some();
        if entry_metadata(&held)?.is_none() {
            return if source_present {
                Ok(())
            } else {
                Err(ResumeRole::HeldOriginal.error(&held, ResumeCheckProblem::Missing))
            };
        }
        if source_present {
            return Err(ResumeRole::Source.error(&paths.source, ResumeCheckProblem::Unexpected));
        }
        stage_contents(&stage, record.stage_identity, ticket.name(), &["source"])?;
        verify_move_source(
            &held,
            ResumeRole::HeldOriginal,
            paths,
            &ticket.receipt.original,
            Some(record.source_identity),
            progress,
        )?;
        progress.checkpoint()?;
        stage_contents(&stage, record.stage_identity, ticket.name(), &["source"])?;
        rename_for_publication(&held, &paths.source)
    })();
    result.map_err(|error| retained_source(&held, error))?;
    Ok(Some(record.source_identity))
}

fn retained_source(held: &Location, error: TransferError) -> TransferError {
    MutationError::RetainedMoveSource {
        held: held.clone(),
        cause: error,
    }
    .into()
}

fn discard_source_stage(ticket: &Ticket, paths: &Paths) -> Result<(), TransferError> {
    if let Some(HeldSource::Verified(record)) = &ticket.holding {
        let stage = staging_at(&paths.source, &record.stage_name)
            .ok_or_else(|| ticket.error(RecoveryRecordProblem::InvalidPaths))?;
        if entry_metadata(&stage)?.is_some() {
            discard_stage(&stage, record.stage_identity, ticket.name(), &[])
                .map_err(|error| retained_source(&stage.join("source"), error))?;
        }
    }
    Ok(())
}

fn record_source_stage(
    ticket: &Ticket,
    held: &Location,
    source_identity: RenamedIdentity,
    cancel: &Cancellation,
) -> Result<(), TransferError> {
    let stage = held
        .parent()
        .ok_or_else(|| ResumeRole::HeldOriginal.error(held, ResumeCheckProblem::InvalidPath))?;
    let identity = stage_identity(&stage)?;
    create_owner_marker(&stage, ticket.name(), cancel)?;
    stage_contents(&stage, identity, ticket.name(), &[])?;
    ticket.record(&Event::Removing(HeldSource::Verified(SourceStage {
        stage_name: stage.name(),
        stage_identity: identity,
        source_identity,
    })))?;
    Ok(())
}

fn finish_move(
    ticket: &Ticket,
    paths: &Paths,
    item: &TransferItem,
    progress: &mut Progress,
    published: impl FnOnce(),
    removed: impl FnOnce(),
    observe: &mut impl FnMut(&Location, Phase) -> Result<(), TransferError>,
) -> Result<(), TransferError> {
    let has_stage = verify_move_stage(ticket, paths, progress)?;
    let history = ticket.removed;
    let scan = progress.cancellation().pausable_scan();
    // Bind the checked destination to the per-entry identities used by deletion.
    let target = if history {
        None
    } else {
        Some(
            TransferSourceManifest::capture_move_observed(&paths.target, &scan, &mut |_, _| {})
                .map_err(|error| manifest_error(ResumeRole::Destination, error))?,
        )
    };
    verify_published_move(ticket, paths, progress)?;
    published();
    if history {
        // This phase only confirms a prior removal. The source path may have been reused.
        removed();
        discard_source_stage(ticket, paths)?;
    } else {
        let verified = (|| {
            let original_access_times = match &ticket.receipt.original {
                Original::File(_) => Some(vec![ticket
                    .receipt
                    .source_accessed
                    .as_ref()
                    .and_then(SavedTime::get)]),
                Original::Folder(_) => ticket.folder_access_times()?,
            };
            let renamed = restore_held_source(ticket, paths, progress)?;
            let source = MoveSource::capture(item, progress)?;
            let source_identity = verify_move_source(
                &paths.source,
                ResumeRole::Source,
                paths,
                &ticket.receipt.original,
                renamed,
                progress,
            )?;
            let source = source.with_original_access_times(&paths.source, original_access_times)?;
            discard_source_stage(ticket, paths)?;
            Ok((source, source_identity))
        })();
        let (source, source_identity) = verified.map_err(|error| {
            progress.results.copied_source_remaining();
            cross_move::incomplete(&paths.target, error, progress)
        })?;
        source.report(item, progress);
        let mut held_root = None;
        let cancel = progress.cancellation();
        cross_move::remove_verified(
            item,
            source,
            target,
            progress,
            removed,
            &mut |location, phase| {
                // Sync the location before rename and the completion after the last unlink.
                if phase == Phase::QuarantinePlanned {
                    record_source_stage(ticket, location, source_identity, &cancel)?;
                    held_root = Some(location.clone());
                } else if phase == Phase::Removed && held_root.as_ref() == Some(location) {
                    ticket
                        .record(&Event::Removed)
                        .map_err(|error| completed_record_error(&paths.target, true, error))?;
                }
                observe(location, phase)
            },
        )?;
    }
    if has_stage {
        let backup = paths.stage.join("previous");
        let extra: &[&str] = if entry_metadata(&backup)?.is_some() {
            let expected = ticket.receipt.previous.as_ref().ok_or_else(|| {
                ResumeRole::ParkedDestination.error(&backup, ResumeCheckProblem::Unexpected)
            })?;
            verify_target(Some(expected), &backup, true, &mut Progress::default())?;
            &["previous"]
        } else {
            &[]
        };
        discard_stage(
            &paths.stage,
            ticket.receipt.stage_identity,
            ticket.name(),
            extra,
        )?;
    }
    finish_record(ticket, paths, progress, true)
}

fn finish_record(
    ticket: &Ticket,
    paths: &Paths,
    progress: &mut Progress,
    completed: bool,
) -> Result<(), TransferError> {
    // Staging has been cleaned up. An unremoved receipt is not a retained payload.
    progress.partial_copy_retained = false;
    ticket.finish().map_err(|error| {
        if completed {
            completed_record_error(&paths.target, ticket.receipt.intent == Intent::Move, error)
        } else {
            error.into()
        }
    })
}

fn completed_record_error(
    target: &Location,
    moving: bool,
    cause: RecoveryRecordError,
) -> TransferError {
    PublicationError::RecoveryFinalization {
        target: target.clone(),
        moving,
        cause,
    }
    .into()
}

pub(crate) fn resume(
    directory: &Path,
    id: &str,
    progress: &mut Progress,
) -> Result<ResumeCompletion, TransferError> {
    let directory = receipts(directory);
    if !journal::valid_name(OsStr::new(id)) {
        return Err(
            RecoveryRecordError::new(&directory, RecoveryRecordProblem::InvalidName).into(),
        );
    }
    let path = directory.join(id);
    let opened = Ticket::open(path.clone()).and_then(|ticket| {
        let ticket =
            ticket.ok_or_else(|| RecoveryRecordError::new(&path, RecoveryRecordProblem::InUse))?;
        let paths = ticket.paths()?;
        Ok((ticket, paths))
    });
    let (ticket, paths) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            let record = Location::Local(path);
            let error = TransferError::from(error);
            progress.results.plan([(record.clone(), None)]);
            progress
                .results
                .record_with_error(&record, Outcome::Failed, Some(&error));
            return Err(error);
        }
    };
    progress
        .results
        .plan([(paths.source.clone(), Some(paths.target.display()))]);
    progress.results.begin(&paths.source);
    progress.set_copy_endpoints(&paths.source, &paths.target);
    progress.partial_copy_retained = true;
    let moving = ticket.receipt.intent == Intent::Move;
    let mut committed = false;
    let mut removed = false;
    let result = (|| {
        let completion = if moving
            && (ticket.holding.is_some() || entry_metadata(&paths.stage.join("payload"))?.is_none())
        {
            Completion::AlreadyPresent
        } else {
            match ticket.receipt.original {
                Original::File(_) => {
                    finish_copy(&ticket, &paths, progress, true, || committed = true)?
                }
                Original::Folder(_) => {
                    tree::resume(&ticket, &paths, progress, || committed = true)?
                }
            }
        };
        if moving {
            let item = TransferItem::new(
                paths.source.clone(),
                paths.source.clone(),
                paths.target.clone(),
            );
            finish_move(
                &ticket,
                &paths,
                &item,
                progress,
                || committed = true,
                || removed = true,
                &mut |_, _| Ok(()),
            )?;
        }
        Ok(completion)
    })();
    let mut reason = result.as_ref().err().map(crate::i18n::file_error);
    let metadata_warning = progress.results.active_metadata_warning();
    let partial_metadata =
        text!("Даты или права доступа сохранены частично. Подробности указаны в строках журнала.");
    let outcome = match (&result, committed) {
        (_, true) if moving && !removed => Outcome::CopiedSourceRemaining,
        (_, true) if result.is_err() || metadata_warning => Outcome::CompletedWithWarning,
        (_, true) => Outcome::Completed,
        (Err(TransferError::Cancelled), false) => {
            progress.results.cancelled = true;
            Outcome::Cancelled
        }
        (Err(TransferError::PublicationUnconfirmed(_)), false) => Outcome::PublicationUnconfirmed,
        (_, false) if progress.partial_copy_retained => Outcome::PartialCopyRetained,
        _ => Outcome::Failed,
    };
    if outcome == Outcome::CompletedWithWarning && reason.is_none() {
        reason = Some(partial_metadata.into());
    }
    progress
        .results
        .record(&paths.source, None, outcome, reason);
    result.map(|kind| ResumeCompletion {
        kind,
        moving,
        metadata_warning,
    })
}

#[derive(Debug)]
pub(crate) struct ResumeCompletion {
    pub(crate) kind: Completion,
    pub(crate) moving: bool,
    pub(crate) metadata_warning: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Completion {
    Copied(u64),
    FolderCopied(u64),
    AlreadyPresent,
}

fn finish_copy(
    ticket: &Ticket,
    paths: &Paths,
    progress: &mut Progress,
    resuming: bool,
    published: impl FnOnce(),
) -> Result<Completion, TransferError> {
    let receipt = &ticket.receipt;
    let Original::File(original) = &receipt.original else {
        return Err(ticket.error(RecoveryRecordProblem::InvalidProof).into());
    };
    let staged = ResumeRole::Staged;
    let owner = ticket.name();
    progress.partial_copy_retained = true;
    let cancel = progress.cancellation();
    let mut committed = false;
    let mut parked = false;
    let backup = paths.stage.join("previous");
    let result = (|| {
        let payload = paths.stage.join("payload");
        if resuming && entry_metadata(&payload)?.is_none() {
            let destination = ResumeRole::Destination;
            if FileProof::read(&paths.target, destination, progress)?
                .0
                .digest
                != original.digest
            {
                return Err(destination.error(&paths.target, ResumeCheckProblem::ContentsMismatch));
            }
            committed = true;
            published();
            return Ok(Completion::AlreadyPresent);
        }
        parked = resuming && receipt.previous.is_some() && entry_metadata(&backup)?.is_some();
        let entries: &[&str] = if parked {
            &["payload", "previous"]
        } else {
            &["payload"]
        };
        stage_contents(&paths.stage, receipt.stage_identity, owner, entries)?;
        if parked {
            verify_target(receipt.previous.as_ref(), &backup, true, progress)?;
            verify_target(None, &paths.target, false, progress)?;
        } else {
            verify_target(receipt.previous.as_ref(), &paths.target, false, progress)?;
        }
        let (partial, metadata) = FileProof::read(&payload, staged, progress)?;
        let offset = partial.stamp.size();
        let size = original.stamp.size();
        if offset > size {
            return Err(staged.error(
                &payload,
                ResumeCheckProblem::SizeMismatch {
                    expected: size,
                    actual: offset,
                },
            ));
        }
        if resuming {
            let source = ResumeRole::Source;
            let actual = FileProof::read(&paths.source, source, progress)?.0;
            if actual.stamp != original.stamp {
                return Err(source.error(&paths.source, ResumeCheckProblem::Changed));
            }
            if actual.digest != original.digest {
                return Err(source.error(&paths.source, ResumeCheckProblem::ContentsMismatch));
            }
            if offset < size {
                check_copy_space(size - offset, &paths.target, &paths.stage)?;
            }
        }
        copy_suffix(
            &paths.source,
            &payload,
            &paths.target,
            original,
            Some((&partial, &metadata)),
            progress,
        )?;
        if FileProof::read(&payload, staged, progress)?.0.digest != original.digest {
            return Err(staged.error(&payload, ResumeCheckProblem::ContentsMismatch));
        }
        let source_metadata = verify_stamp(&paths.source, original.stamp, ResumeRole::Source)?;
        copy_metadata::stage_resumed_entry(
            &paths.source,
            &payload,
            &source_metadata,
            receipt.source_accessed.as_ref().and_then(SavedTime::get),
            receipt.intent.metadata(),
            progress,
        )?;
        verify_stamp(&paths.source, original.stamp, ResumeRole::Source)?;
        stage_contents(&paths.stage, receipt.stage_identity, owner, entries)?;
        if receipt.intent == Intent::Move {
            let metadata = required_metadata(&payload, staged)?;
            let stamp = FileStamp::from(&payload, &metadata, staged)?;
            ticket.seal(RenamedIdentity::File(stamp.ignore_rename_time()))?;
        }
        cancel.check()?;
        if let Some(expected) = &receipt.previous
            && !parked
        {
            let mut preserve = false;
            publish_checked_replacement(
                &payload,
                &paths.target,
                &paths.stage,
                &mut preserve,
                rename_for_publication,
                |candidate| {
                    verify_target(
                        Some(expected),
                        candidate,
                        candidate != &paths.target,
                        progress,
                    )
                },
            )
            .map_err(|error| match error {
                TransferError::Failed(reason) if preserve => {
                    TransferError::PublicationUnconfirmed(reason)
                }
                error => error,
            })?;
        } else {
            if parked {
                verify_target(receipt.previous.as_ref(), &backup, true, progress)?;
            }
            rename_for_publication(&payload, &paths.target)?;
        }
        committed = true;
        published();
        Ok(Completion::Copied(offset))
    })();
    if committed && receipt.intent == Intent::Move {
        return result;
    }
    let cleanup = if committed {
        if matches!(&result, Ok(Completion::AlreadyPresent))
            && entry_metadata(&paths.stage)?.is_none()
        {
            Ok(())
        } else if let Some(previous) = &receipt.previous {
            verify_target(Some(previous), &backup, true, &mut Progress::default())?;
            discard_stage(&paths.stage, receipt.stage_identity, owner, &["previous"])
        } else {
            discard_stage(&paths.stage, receipt.stage_identity, owner, &[])
        }
    } else if matches!(result, Err(TransferError::Cancelled)) {
        if parked {
            stage_contents(
                &paths.stage,
                receipt.stage_identity,
                owner,
                &["payload", "previous"],
            )?;
            verify_target(
                receipt.previous.as_ref(),
                &backup,
                true,
                &mut Progress::default(),
            )?;
            rename_for_publication(&backup, &paths.target)?;
        }
        discard_stage(&paths.stage, receipt.stage_identity, owner, &["payload"])
    } else {
        return result.map_err(|error| retained_error(&paths.stage, error));
    };
    cleanup.map_err(|error| retained_error(&paths.stage, error))?;
    finish_record(ticket, paths, progress, committed)?;
    result
}

fn copy_suffix(
    source: &Location,
    payload: &Location,
    target: &Location,
    original: &FileProof,
    partial: Option<(&FileProof, &EntryMetadata)>,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let cancel = progress.cancellation();
    let role = ResumeRole::Source;
    let (scope, name) = file_scope(source, role)?;
    scope
        .read_checked(name, &cancel, |source, size, input| {
            verify_stamp(source, original.stamp, role)?;
            let mut input = AccessStream::new(input, source);
            let offset = partial.map_or(0, |(proof, _)| proof.stamp.size());
            if offset > size {
                return Err(role.error(source, ResumeCheckProblem::Changed));
            }
            if let Some((proof, _)) = partial.filter(|_| offset != 0) {
                let mut prefix = (&mut input).take(offset);
                progress.set_copy_endpoints(source, source);
                let actual = progress.hash_copy_check(source.path(), offset, &mut prefix)?;
                if actual != Some(proof.digest) {
                    return Err(
                        ResumeRole::Staged.error(payload, ResumeCheckProblem::ContentsMismatch)
                    );
                }
            }
            if offset == size && partial.is_some() {
                return Ok(());
            }
            progress.set_copy_endpoints(source, payload);
            let mut output = match partial {
                Some((_, metadata)) => {
                    StagedFile::open_at(payload, target, offset, metadata, cancel.clone())?
                }
                None => StagedFile::open_for(payload, target, cancel.clone())?,
            };
            let remaining = size - offset;
            let written = progress.copy(
                source.path(),
                Some(remaining),
                &mut input.take(remaining),
                &mut AccessStream::new(&mut output, target),
            )?;
            if written != remaining {
                return Err(role.error(
                    source,
                    ResumeCheckProblem::SizeMismatch {
                        expected: size,
                        actual: offset + written,
                    },
                ));
            }
            output.close()
        })?
        .ok_or_else(|| role.error(source, ResumeCheckProblem::Missing))
}

fn resolved_file(file: &Location, role: ResumeRole) -> Result<Location, TransferError> {
    let invalid = || role.error(file, ResumeCheckProblem::InvalidPath);
    let parent = file.parent().ok_or_else(invalid)?;
    let name = file.path().file_name().ok_or_else(invalid)?;
    let resolved = super::transfer::canonical(&parent)?;
    Ok(parent.with_path(resolved).join(name))
}

fn file_scope(file: &Location, role: ResumeRole) -> Result<(ReadScope, &Path), TransferError> {
    let invalid = || role.error(file, ResumeCheckProblem::InvalidPath);
    let parent = file.parent().ok_or_else(invalid)?;
    let scope = ReadScope::open(&parent)?;
    if scope.parent.location != parent {
        return Err(role.error(file, ResumeCheckProblem::Changed));
    }
    let name = file.path().file_name().map(Path::new).ok_or_else(invalid)?;
    Ok((scope, name))
}

fn required_metadata(file: &Location, role: ResumeRole) -> Result<EntryMetadata, TransferError> {
    entry_metadata(file)?.ok_or_else(|| role.error(file, ResumeCheckProblem::Missing))
}

fn verify_stamp(
    file: &Location,
    stamp: FileStamp,
    role: ResumeRole,
) -> Result<EntryMetadata, TransferError> {
    let current = required_metadata(file, role)?;
    if FileStamp::from(file, &current, role)? != stamp {
        return Err(role.error(file, ResumeCheckProblem::Changed));
    }
    Ok(current)
}

fn verify_target(
    expected: Option<&FileProof>,
    file: &Location,
    parked: bool,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let role = if parked {
        ResumeRole::ParkedDestination
    } else {
        ResumeRole::Destination
    };
    if let Some(expected) = expected {
        let actual = FileProof::read(file, role, progress)?.0;
        let stamp = |value: FileStamp| {
            if parked {
                value.ignore_rename_time()
            } else {
                value
            }
        };
        if stamp(actual.stamp) != stamp(expected.stamp) {
            return Err(role.error(file, ResumeCheckProblem::Changed));
        }
        if actual.digest != expected.digest {
            return Err(role.error(file, ResumeCheckProblem::ContentsMismatch));
        }
    } else if entry_metadata(file)?.is_some() {
        return Err(role.error(file, ResumeCheckProblem::Unexpected));
    }
    Ok(())
}

fn stage_identity(stage: &Location) -> Result<Option<(u64, u64)>, TransferError> {
    let role = ResumeRole::Staged;
    match required_metadata(stage, role)? {
        EntryMetadata::Local(attr) if attr.is_dir() => Ok(Some((attr.dev(), attr.ino()))),
        EntryMetadata::Remote(attr) if attr.is_dir() => Ok(None),
        EntryMetadata::Remote(attr) if attr.perm.is_none() => {
            Err(role.error(stage, ResumeCheckProblem::IncompleteMetadata))
        }
        _ => Err(role.error(stage, ResumeCheckProblem::WrongType)),
    }
}

fn stage_contents(
    stage: &Location,
    identity: Option<(u64, u64)>,
    owner: &str,
    extra: &[&str],
) -> Result<(), TransferError> {
    if stage_identity(stage)? != identity || super::transfer::canonical(stage)? != stage.path() {
        return Err(StagingError::IdentityChanged(stage.clone()).into());
    }
    let role = ResumeRole::Staged;
    let action = AccessAction::ReadChildren;
    let expected: BTreeSet<OsString> = std::iter::once(owner)
        .chain(extra.iter().copied())
        .map(OsString::from)
        .collect();
    let contents: BTreeSet<OsString> = if let Some(path) = stage.local() {
        fs::read_dir(path)
            .map_err(|error| AccessError::new(stage, action, error))?
            .take(expected.len() + 1)
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<_, _>>()
            .map_err(|error| AccessError::new(stage, action, error))?
    } else {
        let mut directory = remote(stage)
            .map_err(|error| AccessError::new(stage, action, error))?
            .opendir(stage.path())
            .map_err(|error| AccessError::new(stage, action, error))?;
        let mut names = BTreeSet::new();
        let mut exhausted = false;
        for _ in 0..expected.len() + 3 {
            let Some((name, _)) = super::listing::next_sftp_entry(&mut directory)
                .map_err(|error| AccessError::new(stage, action, error))?
            else {
                exhausted = true;
                break;
            };
            if name == Path::new(".") || name == Path::new("..") {
                continue;
            }
            if !expected.contains(name.as_os_str()) || !names.insert(name.into_os_string()) {
                return Err(role.error(stage, ResumeCheckProblem::Unexpected));
            }
        }
        if !exhausted {
            return Err(role.error(stage, ResumeCheckProblem::Unexpected));
        }
        names
    };
    if let Some(name) = contents.difference(&expected).next() {
        return Err(role.error(&stage.join(name), ResumeCheckProblem::Unexpected));
    }
    if let Some(name) = expected.difference(&contents).next() {
        return Err(role.error(&stage.join(name), ResumeCheckProblem::Missing));
    }
    let marker = stage.join(owner);
    let metadata = required_metadata(&marker, role)?;
    let actual = FileStamp::from(&marker, &metadata, role)?.size();
    if actual != 0 {
        return Err(role.error(
            &marker,
            ResumeCheckProblem::SizeMismatch {
                expected: 0,
                actual,
            },
        ));
    }
    Ok(())
}

fn discard_stage(
    stage: &Location,
    identity: Option<(u64, u64)>,
    owner: &str,
    extra: &[&str],
) -> Result<(), TransferError> {
    stage_contents(stage, identity, owner, extra)?;
    for name in extra {
        let item = stage.join(name);
        if !required_metadata(&item, ResumeRole::Staged)?.is_file() {
            return Err(ResumeRole::Staged.error(&item, ResumeCheckProblem::WrongType));
        }
    }
    if let Some(path) = stage.local() {
        for name in extra.iter().copied().chain([owner]) {
            fs::remove_file(path.join(name))?;
        }
        fs::remove_dir(path)?;
    } else {
        let sftp = remote(stage)?;
        for name in extra.iter().copied().chain([owner]) {
            sftp.unlink(stage.join(name).path()).map_err(super::err)?;
        }
        sftp.rmdir(stage.path()).map_err(super::err)?;
    }
    Ok(())
}

fn manifest_error(role: ResumeRole, error: TransferError) -> TransferError {
    if let TransferError::Failed(Failure::Mutation(reason)) = &error
        && let MutationError::SourceChanged(location) = reason.as_ref()
    {
        return role.error(location, ResumeCheckProblem::Changed);
    }
    error
}

fn retained_error(stage: &Location, error: TransferError) -> TransferError {
    error.with_retained_staging(stage, RecoveryNotice::ResumeCopy)
}
