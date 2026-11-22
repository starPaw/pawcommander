//! Folder recovery verifies a fresh snapshot against the recorded fingerprint.
//! Moves retain original access times separately because reading changes them.
use super::{
    checked_digest, copy_metadata, copy_suffix, discard_empty_stage, discard_stage, entry_metadata,
    file_scope, finish_cancelled_cleanup, finish_record, manifest_error, register_copy,
    remove_staged_entry, rename_for_publication, required_metadata, retained_error, stage_contents,
    stage_identity, verify_stamp, verify_target, Cancellation, Completion, EntryMetadata,
    FileProof, Intent, Location, Original, Paths, Progress, Receipt, RenamedIdentity, SavedTime,
    Ticket, TransferError,
};
use crate::file_error::{
    AccessAction, AccessError, PublicationError, RecoveryNotice, RecoveryRecordProblem,
    ResumeCheckProblem, ResumeRole,
};
use crate::fs_ops::local_copy_write_error;
use crate::operations::AccessStream;
use crate::vfs::{
    sftp::remote,
    transfer::{
        check_copy_space, source_children, with_preserved_staging, TransferSourceEntry,
        TransferSourceManifest, MAX_MOVE_JOURNAL_ENTRIES,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Proof {
    manifest: [u8; 32],
    contents: [u8; 32],
}

enum Content {
    Directory,
    File(FileProof),
    Link(PathBuf),
}

struct Node {
    entry: TransferSourceEntry,
    content: Content,
}

struct Snapshot {
    entries: BTreeMap<PathBuf, Node>,
    proof: Proof,
    bytes: u64,
    local_names: bool,
}

type Inventory = BTreeMap<PathBuf, EntryMetadata>;
type Partials = BTreeMap<PathBuf, (FileProof, EntryMetadata)>;

impl Snapshot {
    fn capture(
        root: &Location,
        role: ResumeRole,
        local_names: bool,
        progress: &mut Progress,
    ) -> Result<Self, TransferError> {
        let cancel = progress.cancellation().pausable_scan();
        let listing = TransferSourceManifest::capture_copy_tree(root, &cancel, local_names)
            .map_err(|error| manifest_error(role, error))?;
        if !listing
            .entries
            .get(root)
            .is_some_and(|entry| entry.children.is_some())
        {
            return Err(role.error(root, ResumeCheckProblem::WrongType));
        }
        let mut entries = BTreeMap::new();
        let mut manifest = Sha256::new();
        let mut contents = Sha256::new();
        for (source, entry) in listing.entries {
            progress.checkpoint()?;
            let relative = source
                .path()
                .strip_prefix(root.path())
                .map_err(|_| role.error(&source, ResumeCheckProblem::InvalidPath))?;
            let content = if entry.children.is_some() {
                Content::Directory
            } else if entry.metadata.is_file() {
                let (proof, actual) = FileProof::read(&source, role, progress)?;
                entry
                    .verify_metadata(&source, &actual)
                    .map_err(|error| manifest_error(role, error))?;
                Content::File(proof)
            } else if local_names
                && matches!(&entry.metadata, EntryMetadata::Local(meta) if meta.is_symlink())
            {
                Content::Link(read_link(&source)?)
            } else {
                return Err(role.error(&source, ResumeCheckProblem::WrongType));
            };
            feed_field(&mut manifest, relative.as_os_str().as_encoded_bytes());
            feed_field(&mut manifest, &metadata_bytes(&entry.metadata, false)?);
            feed_field(&mut contents, relative.as_os_str().as_encoded_bytes());
            match &content {
                Content::Directory => contents.update([0]),
                Content::File(proof) => {
                    contents.update([1]);
                    contents.update(proof.digest);
                }
                Content::Link(destination) => {
                    contents.update([2]);
                    feed_field(&mut contents, destination.as_os_str().as_encoded_bytes());
                }
            }
            entries.insert(relative.to_owned(), Node { entry, content });
        }
        let snapshot = Self {
            entries,
            proof: Proof {
                manifest: manifest.finalize().into(),
                contents: contents.finalize().into(),
            },
            bytes: listing.bytes,
            local_names,
        };
        snapshot.verify(root, role, &cancel)?;
        Ok(snapshot)
    }

    fn verify(
        &self,
        root: &Location,
        role: ResumeRole,
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        for (relative, node) in &self.entries {
            let source = at(root, relative);
            node.entry
                .verify(&source, cancel, self.local_names)
                .map_err(|error| manifest_error(role, error))?;
            if let Content::Link(expected) = &node.content
                && read_link(&source)? != *expected
            {
                return Err(role.error(&source, ResumeCheckProblem::ContentsMismatch));
            }
        }
        Ok(())
    }

    fn renamed_identity(&self) -> Result<RenamedIdentity, TransferError> {
        publication_seal(
            self.entries
                .iter()
                .map(|(path, node)| (path, &node.entry.metadata)),
        )
    }

    fn report(&self, paths: &Paths, progress: &mut Progress) {
        progress.results.descendants(
            &paths.source,
            &paths.target,
            self.entries
                .keys()
                .map(|relative| at(&paths.source, relative)),
        );
    }

    /// Inspect the entire draft before extending any entry. A missing parent
    /// implies all of its descendants are absent from the draft.
    fn inventory(
        &self,
        payload: &Location,
        complete: bool,
        cancel: &Cancellation,
    ) -> Result<Inventory, TransferError> {
        let mut present = Inventory::new();
        let mut discovered = BTreeSet::from([PathBuf::new()]);
        let role = ResumeRole::Staged;
        for (relative, node) in &self.entries {
            cancel.check()?;
            if !discovered.remove(relative) {
                if complete {
                    return Err(role.error(&at(payload, relative), ResumeCheckProblem::Missing));
                }
                continue;
            }
            let copy = at(payload, relative);
            let actual = required_metadata(&copy, role)?;
            match (&node.content, &actual) {
                (Content::Directory, actual) if actual.is_directory() => {
                    let expected = node
                        .entry
                        .children
                        .as_ref()
                        .expect("directory has children");
                    let names = source_children(&copy, cancel, self.local_names)?;
                    if let Some(name) = names
                        .iter()
                        .find(|name| expected.binary_search(name).is_err())
                    {
                        return Err(role.error(&copy.join(name), ResumeCheckProblem::Unexpected));
                    }
                    if complete
                        && names.len() != expected.len()
                        && let Some(name) = expected
                            .iter()
                            .find(|name| names.binary_search(name).is_err())
                    {
                        return Err(role.error(&copy.join(name), ResumeCheckProblem::Missing));
                    }
                    discovered.extend(names.into_iter().map(|name| relative.join(name)));
                }
                (Content::File(_), EntryMetadata::Local(meta))
                    if meta.is_file() && meta.nlink() == 1 => {}
                (Content::File(_), EntryMetadata::Local(meta)) if meta.is_file() => {
                    return Err(role.error(&copy, ResumeCheckProblem::Changed));
                }
                (Content::File(_), EntryMetadata::Remote(stat)) if stat.is_file() => {}
                (Content::Link(expected), EntryMetadata::Local(meta)) if meta.is_symlink() => {
                    if read_link(&copy)? != *expected {
                        return Err(role.error(&copy, ResumeCheckProblem::ContentsMismatch));
                    }
                }
                _ => return Err(role.error(&copy, ResumeCheckProblem::WrongType)),
            }
            present.insert(relative.clone(), actual);
        }
        Ok(present)
    }

    fn verify_bytes(
        &self,
        paths: &Paths,
        payload: &Location,
        present: &Inventory,
        complete: bool,
        progress: &mut Progress,
    ) -> Result<(Partials, u64), TransferError> {
        let mut partials = Partials::new();
        let mut reused = 0;
        let role = ResumeRole::Staged;
        for (relative, metadata) in present {
            let Content::File(original) = &self.entries[relative].content else {
                continue;
            };
            let source = at(&paths.source, relative);
            let copy = at(payload, relative);
            let (proof, actual) = FileProof::read(&copy, role, progress)?;
            let size = proof.stamp.size();
            let expected = original.stamp.size();
            if !metadata.matches_file(&actual) {
                return Err(role.error(&copy, ResumeCheckProblem::Changed));
            }
            if size > expected || (complete && size < expected) {
                return Err(role.error(
                    &copy,
                    ResumeCheckProblem::SizeMismatch {
                        expected,
                        actual: size,
                    },
                ));
            }
            let matches = if size == expected {
                proof.digest == original.digest
            } else {
                size == 0 || check_prefix(&source, original, &proof, progress)?
            };
            if !matches {
                return Err(role.error(&copy, ResumeCheckProblem::ContentsMismatch));
            }
            reused += size;
            partials.insert(relative.clone(), (proof, actual));
        }
        Ok((partials, reused))
    }

    fn extend(
        &self,
        paths: &Paths,
        payload: &Location,
        mut inventory: Inventory,
        partials: &Partials,
        progress: &mut Progress,
    ) -> Result<(), TransferError> {
        let cancel = progress.cancellation();
        let role = ResumeRole::Staged;
        for (relative, node) in &self.entries {
            progress.checkpoint()?;
            let source = at(&paths.source, relative);
            let copy = at(payload, relative);
            let target = at(&paths.target, relative);
            let existing = inventory.contains_key(relative);
            if let Some(parent) = relative.parent() {
                let directory = at(payload, parent);
                check_directory(
                    &directory,
                    inventory
                        .get(parent)
                        .ok_or_else(|| role.error(&directory, ResumeCheckProblem::Missing))?,
                )?;
            }
            let result = match &node.content {
                Content::Directory if !existing => mkdir(&copy, &target).and_then(|()| {
                    let metadata = required_metadata(&copy, role)?;
                    if !metadata.is_directory() {
                        return Err(role.error(&copy, ResumeCheckProblem::WrongType));
                    }
                    inventory.insert(relative.clone(), metadata);
                    Ok(())
                }),
                Content::Link(destination) if !existing => {
                    std::os::unix::fs::symlink(destination, copy.path()).map_err(|error| {
                        local_copy_write_error(&target, AccessAction::CreateLink, error)
                    })
                }
                Content::File(original) => {
                    let partial = partials
                        .get(relative)
                        .map(|(proof, metadata)| (proof, metadata));
                    if partial.is_some_and(|(file, _)| file.stamp.size() == original.stamp.size()) {
                        Ok(())
                    } else {
                        copy_suffix(&source, &copy, &target, original, partial, progress)
                    }
                }
                _ => Ok(()),
            };
            progress.results.staged(
                &source,
                result.as_ref().err().map(crate::i18n::file_error),
                matches!(&result, Err(TransferError::Cancelled)),
            );
            result?;
            cancel.check()?;
        }
        Ok(())
    }

    fn apply_metadata(
        &self,
        paths: &Paths,
        payload: &Location,
        mode: copy_metadata::Mode,
        original_access_times: Option<&[Option<SystemTime>]>,
        progress: &mut Progress,
    ) -> Result<(), TransferError> {
        for (index, (relative, node)) in self.entries.iter().enumerate().rev() {
            progress.checkpoint()?;
            if !matches!(node.content, Content::Link(_)) {
                let source = at(&paths.source, relative);
                let copy = at(payload, relative);
                let target = at(&paths.target, relative);
                let metadata = &node.entry.metadata;
                let accessed = if mode == copy_metadata::Mode::KeepReadableForMove {
                    original_access_times.and_then(|times| times[index])
                } else {
                    copy_metadata::access_time(metadata)
                };
                copy_metadata::stage_resumed_entry(
                    &source, &copy, &target, metadata, accessed, mode, progress,
                )?;
            }
        }
        Ok(())
    }

    fn discard(&self, payload: &Location, present: &Inventory) -> Result<(), TransferError> {
        let role = ResumeRole::Staged;
        for (relative, recorded) in present.iter().rev() {
            let copy = at(payload, relative);
            if let Some(parent) = relative.parent() {
                let directory = at(payload, parent);
                check_directory(
                    &directory,
                    present
                        .get(parent)
                        .ok_or_else(|| role.error(&directory, ResumeCheckProblem::Missing))?,
                )?;
            }
            let current = required_metadata(&copy, role)?;
            let directory = recorded.is_directory();
            if directory {
                if !current.is_directory()
                    || (copy.local().is_some() && !recorded.same_object(&current))
                {
                    return Err(role.error(&copy, ResumeCheckProblem::Changed));
                }
            } else {
                // Never follow a staged symbolic link or remove a replacement directory.
                let same = if copy.local().is_some() {
                    !current.is_directory() && recorded.same_object(&current)
                } else {
                    recorded.matches_file(&current)
                };
                if !same {
                    return Err(role.error(&copy, ResumeCheckProblem::Changed));
                }
            }
            remove_staged_entry(&copy, directory)?;
        }
        Ok(())
    }
}

pub(super) fn start(
    source: Location,
    target: Location,
    directory: &Path,
    intent: Intent,
    progress: &mut Progress,
    published: impl FnOnce(),
    after: impl FnOnce(&Ticket, &Paths, &mut Progress) -> Result<(), TransferError>,
) -> Result<(), TransferError> {
    let local_names = !source.remote() && !target.remote();
    verify_target(None, &target, false, progress)?;
    let snapshot = Snapshot::capture(&source, ResumeRole::Source, local_names, progress)?;
    let original_access_times = if intent == Intent::Move {
        if snapshot.entries.len() > MAX_MOVE_JOURNAL_ENTRIES {
            return Err(crate::i18n::text!(
                "Пофайловый журнал переноса превысил лимит 100000 элементов"
            )
            .into());
        }
        Some(
            snapshot
                .entries
                .values()
                .map(|node| copy_metadata::access_time(&node.entry.metadata).map(SavedTime::from))
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };
    let cancel = progress.cancellation();
    with_preserved_staging(&target, &cancel, |stage, preserve| {
        *preserve = false;
        check_copy_space(snapshot.bytes, &target, stage)?;
        mkdir(&stage.join("payload"), &target)?;
        let record = Receipt {
            intent,
            source: (&source).into(),
            target: (&target).into(),
            stage_name: stage.name(),
            stage_identity: stage_identity(stage)?,
            original: Original::Folder(snapshot.proof),
            source_accessed: None,
            folder_access_times: None,
            previous: None,
        };
        let ticket = register_copy(
            directory,
            stage,
            record,
            original_access_times.as_deref(),
            &cancel,
        )?;
        *preserve = true;
        let paths = ticket.paths()?;
        finish(&ticket, &paths, Some(snapshot), progress, published)?;
        after(&ticket, &paths, progress)
    })
}

pub(super) fn resume(
    ticket: &Ticket,
    paths: &Paths,
    progress: &mut Progress,
    published: impl FnOnce(),
) -> Result<Completion, TransferError> {
    finish(ticket, paths, None, progress, published)
}

fn finish(
    ticket: &Ticket,
    paths: &Paths,
    mut snapshot: Option<Snapshot>,
    progress: &mut Progress,
    published: impl FnOnce(),
) -> Result<Completion, TransferError> {
    let receipt = &ticket.receipt;
    let Original::Folder(proof) = receipt.original else {
        return Err(ticket.error(RecoveryRecordProblem::InvalidProof).into());
    };
    if receipt.previous.is_some() {
        return Err(ticket.error(RecoveryRecordProblem::InvalidProof).into());
    }
    progress.partial_copy_retained = true;
    let owner = ticket.name();
    let payload = paths.stage.join("payload");
    let cancel = progress.cancellation();
    let mut committed = false;
    let mut stage_checked = false;
    let mut draft_verified = false;
    let local_names = !paths.source.remote() && !paths.target.remote();
    let result = (|| {
        if snapshot.is_none() && entry_metadata(&payload)?.is_none() {
            verify_published(paths, proof, None, progress)?;
            committed = true;
            published();
            return Ok(Completion::AlreadyPresent);
        }
        stage_contents(&paths.stage, receipt.stage_identity, owner, &["payload"])?;
        stage_checked = true;
        verify_target(None, &paths.target, false, progress)?;
        let resuming = snapshot.is_none();
        if resuming {
            let current =
                Snapshot::capture(&paths.source, ResumeRole::Source, local_names, progress)?;
            if current.proof != proof {
                let problem = if current.proof.contents != proof.contents {
                    ResumeCheckProblem::ContentsMismatch
                } else {
                    ResumeCheckProblem::Changed
                };
                return Err(ResumeRole::Source.error(&paths.source, problem));
            }
            snapshot = Some(current);
        }
        let snapshot = snapshot.as_ref().expect("verified source tree");
        snapshot.report(paths, progress);
        let draft = snapshot.inventory(&payload, false, &cancel)?;
        let (partials, reused) = snapshot.verify_bytes(paths, &payload, &draft, false, progress)?;
        let original_access_times = ticket.folder_access_times()?;
        if original_access_times
            .as_ref()
            .is_some_and(|times| times.len() != snapshot.entries.len())
        {
            return Err(ticket
                .error(RecoveryRecordProblem::InvalidAccessTimes)
                .into());
        }
        draft_verified = true;
        if resuming && reused < snapshot.bytes {
            check_copy_space(snapshot.bytes - reused, &paths.target, &paths.stage)?;
        }
        snapshot.extend(paths, &payload, draft, &partials, progress)?;
        let complete = snapshot.inventory(&payload, true, &cancel)?;
        snapshot.verify_bytes(paths, &payload, &complete, true, progress)?;
        snapshot.verify(&paths.source, ResumeRole::Source, &cancel)?;
        snapshot.apply_metadata(
            paths,
            &payload,
            receipt.intent.metadata(),
            original_access_times.as_deref(),
            progress,
        )?;
        stage_contents(&paths.stage, receipt.stage_identity, owner, &["payload"])?;
        snapshot.verify(&paths.source, ResumeRole::Source, &cancel)?;
        if receipt.intent == Intent::Move {
            let complete = snapshot.inventory(&payload, true, &cancel)?;
            ticket.seal(publication_seal(&complete)?)?;
        }
        cancel.check()?;
        rename_for_publication(&payload, &paths.target)?;
        committed = true;
        published();
        Ok(Completion::FolderCopied(reused))
    })();
    if committed {
        if receipt.intent == Intent::Move {
            return result;
        }
        if !matches!(&result, Ok(Completion::AlreadyPresent))
            || entry_metadata(&paths.stage)?.is_some()
        {
            discard_stage(&paths.stage, receipt.stage_identity, owner, &[])?;
        }
    } else if matches!(&result, Err(TransferError::Cancelled)) {
        progress.results.cancelled = true;
        let Some(snapshot) = snapshot.filter(|_| draft_verified) else {
            progress.partial_copy_retained = stage_checked;
            return if stage_checked {
                Err(PublicationError::Retained {
                    staging: paths.stage.clone(),
                    notice: RecoveryNotice::InspectStaging,
                    cause: TransferError::Cancelled,
                }
                .into())
            } else {
                result
            };
        };
        let cleanup = (|| {
            stage_contents(&paths.stage, receipt.stage_identity, owner, &["payload"])?;
            let present = snapshot.inventory(&payload, false, &Cancellation::default())?;
            snapshot.verify_bytes(paths, &payload, &present, false, &mut Progress::default())?;
            snapshot.discard(&payload, &present)?;
            discard_empty_stage(ticket, paths, progress)
        })();
        return finish_cancelled_cleanup(ticket, paths, cleanup);
    } else {
        return result.map_err(|error| retained_error(&paths.stage, error));
    }
    finish_record(ticket, paths, progress)?;
    result
}

pub(super) fn verify_move_source(
    source: &Location,
    role: ResumeRole,
    destination: &Location,
    proof: Proof,
    renamed: Option<RenamedIdentity>,
    progress: &mut Progress,
) -> Result<RenamedIdentity, TransferError> {
    let snapshot = Snapshot::capture(
        source,
        role,
        !source.remote() && !destination.remote(),
        progress,
    )?;
    let identity = snapshot.renamed_identity()?;
    let matches = match renamed {
        Some(expected) => identity == expected,
        None => snapshot.proof.manifest == proof.manifest,
    };
    if !matches {
        return Err(role.error(source, ResumeCheckProblem::Changed));
    }
    if snapshot.proof.contents != proof.contents {
        return Err(role.error(source, ResumeCheckProblem::ContentsMismatch));
    }
    Ok(identity)
}

pub(super) fn verify_published(
    paths: &Paths,
    proof: Proof,
    seal: Option<RenamedIdentity>,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let local_names = !paths.source.remote() && !paths.target.remote();
    let role = ResumeRole::Destination;
    let snapshot = Snapshot::capture(&paths.target, role, local_names, progress)?;
    if snapshot.proof.contents != proof.contents {
        return Err(role.error(&paths.target, ResumeCheckProblem::ContentsMismatch));
    }
    if let Some(expected) = seal
        && snapshot.renamed_identity()? != expected
    {
        return Err(role.error(&paths.target, ResumeCheckProblem::Changed));
    }
    Ok(())
}

fn at(root: &Location, relative: &Path) -> Location {
    if relative.as_os_str().is_empty() {
        root.clone()
    } else {
        root.join(relative)
    }
}

fn mkdir(location: &Location, report_at: &Location) -> Result<(), TransferError> {
    let action = AccessAction::CreateDirectory;
    if let Some(path) = location.local() {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|error| local_copy_write_error(report_at, action, error))?;
    } else {
        remote(location)
            .map_err(|error| AccessError::new(report_at, action, error))?
            .mkdir(location.path(), 0o700)
            .map_err(|error| AccessError::new(report_at, action, error))?;
    }
    Ok(())
}

fn read_link(location: &Location) -> Result<PathBuf, TransferError> {
    fs::read_link(location.path())
        .map_err(|source| AccessError::new(location, AccessAction::ReadLink, source).into())
}

fn check_directory(location: &Location, recorded: &EntryMetadata) -> Result<(), TransferError> {
    let role = ResumeRole::Staged;
    let current = required_metadata(location, role)?;
    let same = match (recorded, &current) {
        (EntryMetadata::Local(_), EntryMetadata::Local(now)) => {
            now.is_dir() && recorded.same_object(&current)
        }
        (EntryMetadata::Remote(before), EntryMetadata::Remote(now)) => {
            now.is_dir()
                && before.perm == now.perm
                && before.uid == now.uid
                && before.gid == now.gid
        }
        _ => false,
    };
    if !same || super::super::transfer::canonical(location)? != location.path() {
        return Err(role.error(location, ResumeCheckProblem::Changed));
    }
    Ok(())
}

fn check_prefix(
    source: &Location,
    original: &FileProof,
    partial: &FileProof,
    progress: &mut Progress,
) -> Result<bool, TransferError> {
    let cancel = progress.cancellation();
    let role = ResumeRole::Source;
    let (scope, name) = file_scope(source, role)?;
    scope
        .read_checked(name, &cancel, |path, _, input| {
            verify_stamp(path, original.stamp, role)?;
            let size = partial.stamp.size();
            progress.set_copy_endpoints(path, path);
            let mut input = AccessStream::new(input, path).take(size);
            Ok(checked_digest(progress, path, size, role, &mut input)? == partial.digest)
        })?
        .ok_or_else(|| role.error(source, ResumeCheckProblem::Missing))
}

fn feed_field(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

fn publication_seal<'a>(
    entries: impl IntoIterator<Item = (&'a PathBuf, &'a EntryMetadata)>,
) -> Result<RenamedIdentity, TransferError> {
    let mut digest = Sha256::new();
    for (path, metadata) in entries {
        feed_field(&mut digest, path.as_os_str().as_encoded_bytes());
        feed_field(
            &mut digest,
            &metadata_bytes(metadata, path.as_os_str().is_empty())?,
        );
    }
    Ok(RenamedIdentity::Folder(digest.finalize().into()))
}

fn metadata_bytes(metadata: &EntryMetadata, renamed_root: bool) -> Result<Vec<u8>, TransferError> {
    match metadata {
        EntryMetadata::Local(meta) => serde_json::to_vec(&(
            meta.dev(),
            meta.ino(),
            meta.mode(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            if renamed_root { 0 } else { meta.ctime() },
            if renamed_root { 0 } else { meta.ctime_nsec() },
        )),
        EntryMetadata::Remote(stat) => {
            serde_json::to_vec(&(stat.perm, stat.size, stat.mtime, stat.uid, stat.gid))
        }
    }
    .map_err(|error| error.to_string().into())
}
