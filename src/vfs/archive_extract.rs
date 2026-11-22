use super::{
    sftp::remote,
    staged_file::StagedFile,
    transfer::{
        canonical, entry_kind, entry_metadata, keep_both_name, publish_item_typed,
        with_staging_typed, EntryMetadata,
    },
    undo::Parent,
    Location,
};
use crate::{
    archive::{Extraction, SelectedEntry, SelectedTree},
    conflict::{Answer, Conflict, Decision, EntryDetails},
    file_error::{ArchiveError, LocalPathAction, MutationError, ReplacementRefusal},
    fs_ops,
    operations::{Progress, TransferError},
    result_history::Outcome,
};
use std::{collections::HashSet, fs};
#[cfg(target_os = "macos")]
use {super::archive_smb, crate::file_error::SmbMutationAction};

#[cfg(test)]
mod tests;

pub(crate) fn extract_archive(
    sources: &[Location],
    directory: &Location,
    password: Option<&str>,
    progress: &mut Progress,
    decide: impl FnMut(Conflict) -> Result<Answer, TransferError>,
) -> Result<ExtractionSummary, ExtractionFailure> {
    let mut summary = ExtractionSummary {
        selected: sources.len(),
        ..ExtractionSummary::default()
    };
    let result = extract(sources, directory, password, progress, decide, &mut summary);
    match result {
        Ok(()) => Ok(summary),
        Err(error) => {
            if matches!(error, TransferError::Cancelled) {
                progress.results.cancelled = true;
            }
            Err(ExtractionFailure { summary, error })
        }
    }
}

#[derive(Debug)]
pub(crate) struct ExtractionFailure {
    pub(crate) summary: ExtractionSummary,
    pub(crate) error: TransferError,
}

#[derive(Debug, Default)]
pub(crate) struct ExtractionSummary {
    selected: usize,
    pub(crate) completed: usize,
    pub(crate) skipped: usize,
}

impl ExtractionSummary {
    pub(crate) fn remaining(&self) -> usize {
        self.selected - self.completed - self.skipped
    }
}

fn extract(
    sources: &[Location],
    directory: &Location,
    password: Option<&str>,
    progress: &mut Progress,
    mut decide: impl FnMut(Conflict) -> Result<Answer, TransferError>,
    totals: &mut ExtractionSummary,
) -> Result<(), TransferError> {
    if directory.is_archive() {
        return Err(MutationError::ReadOnlyArchive(directory.clone()).into());
    }
    progress.results.plan(sources.iter().map(|source| {
        (
            source.clone(),
            source
                .path()
                .file_name()
                .map(|name| directory.join(name).display()),
        )
    }));
    let cancel = progress.cancellation();
    #[cfg(target_os = "macos")]
    let mut mount = {
        cancel.check()?;
        archive_smb::guard(directory, SmbMutationAction::ExtractArchive)?
    };
    if let Some(path) = sources.first().and_then(Location::archive_source) {
        progress.item(path);
    }
    let (mut archive, trees) = Extraction::prepare(sources, password, &cancel)?;
    totals.selected = trees.len();
    let parent = Parent::capture(directory)?;
    #[cfg(target_os = "macos")]
    let smb_destination = archive_smb::is_destination(&mut mount, &parent.location)?;
    let targets = trees
        .iter()
        .map(|tree| {
            parent
                .location
                .join(tree.source.path().file_name().expect("validated selection"))
        })
        .collect::<Vec<_>>();
    let mut reserved: HashSet<_> = targets.iter().cloned().collect();
    let mut count = 0;
    for target in &targets {
        cancel.check()?;
        #[cfg(target_os = "macos")]
        if !mount.is_owned() {
            archive_smb::check(&mut mount, target)?;
        }
        count += usize::from(entry_metadata(target)?.is_some());
    }
    #[cfg(target_os = "macos")]
    let smb_destination = smb_destination || mount.is_owned();
    let mut index = 0;
    let mut group = None;
    for (tree, mut target) in trees.into_iter().zip(targets) {
        cancel.check()?;
        progress.item(tree.source.path());
        progress.results.begin(&tree.source);
        #[cfg(target_os = "macos")]
        mount.recheck()?;
        verify_destination(&parent, directory)?;
        archive.verify_source()?;
        let mut replace = None;
        if let Some(existing) = entry_metadata(&target)? {
            index += 1;
            let first = &tree.entries[0];
            let replacement_refusal = if first.is_dir() {
                Some(ReplacementRefusal::SourceNotRegular)
            } else if !existing.is_file() {
                Some(ReplacementRefusal::TargetNotRegular)
            } else {
                let would_overwrite_archive = match &existing {
                    EntryMetadata::Local(metadata) => {
                        archive.is_source_file(target.path(), metadata)
                    }
                    EntryMetadata::Remote(_) => tree.source.archive_source() == Some(target.path()),
                };
                would_overwrite_archive.then_some(ReplacementRefusal::WouldOverwriteArchive)
            };
            #[cfg(target_os = "macos")]
            let replacement_refusal = if smb_destination {
                Some(ReplacementRefusal::Smb)
            } else {
                replacement_refusal
            };
            let decision = if let Some(saved) =
                group.filter(|&saved| saved != Decision::Replace || replacement_refusal.is_none())
            {
                saved
            } else {
                progress.waiting_for_conflict(tree.source.path());
                let answer = decide(Conflict {
                    source: EntryDetails {
                        location: tree.source.clone(),
                        kind: entry_kind(!first.is_dir(), first.is_dir(), false),
                        size: (!first.is_dir()).then_some(first.size),
                        modified: first.modified,
                    },
                    target: existing.details(&target),
                    replacement_refusal,
                    index,
                    count: count.max(index),
                })?;
                cancel.check()?;
                progress.item(tree.source.path());
                if answer.apply_to_group {
                    group = Some(answer.decision);
                }
                answer.decision
            };
            match decision {
                Decision::Skip => {
                    progress.results.record(
                        &tree.source,
                        Some(target.display()),
                        Outcome::Skipped,
                        None,
                    );
                    totals.skipped += 1;
                    continue;
                }
                Decision::Cancel => return Err(TransferError::Cancelled),
                Decision::KeepBoth => {
                    #[cfg(target_os = "macos")]
                    {
                        mount.recheck()?;
                        archive.verify_source()?;
                    }
                    target = keep_both_name(&target, &reserved, &cancel)?;
                    reserved.insert(target.clone());
                }
                Decision::Replace if replacement_refusal.is_none() => replace = Some(existing),
                Decision::Replace => {
                    return Err(ArchiveError::ExtractionReplaceUnavailable(target).into())
                }
            }
        }
        progress.results.target(&tree.source, target.display());
        let mut published = false;
        let result = (|| {
            #[cfg(target_os = "macos")]
            if smb_destination {
                mount.recheck()?;
                archive.verify_source()?;
                let entry = &tree.entries[0];
                if entry.is_dir() {
                    return Err(ArchiveError::SmbExtractionFolder {
                        entry: tree.source.clone(),
                        destination: target.clone(),
                    }
                    .into());
                }
                let staged = archive_smb::spool(&target, &cancel, |output| {
                    unpack_file(&mut archive, entry, &tree.source, &target, output, progress)
                })?;
                archive.verify_source()?;
                verify_destination(&parent, directory)?;
                archive_smb::publish(staged, &tree.source, &target, &parent, &mount, progress)?;
                published = true;
                return Ok(());
            }
            if let Some(path) = parent.location.local() {
                fs_ops::check_local_copy_space(tree.bytes, path)?;
            }
            with_staging_typed(&target, Some(&cancel), |staging, _| {
                let payload = staging.join("payload");
                unpack_tree(&mut archive, &tree, &payload, &target, progress)?;
                progress.item(target.path());
                archive.verify_source()?;
                verify_destination(&parent, directory)?;
                cancel.check()?;
                publish_item_typed(
                    &payload,
                    &target,
                    replace.as_ref(),
                    |_| {},
                    || Ok(()),
                    || published = true,
                )
            })
        })();
        let outcome = match &result {
            Ok(()) => Outcome::Completed,
            Err(_) if published => Outcome::CompletedWithWarning,
            Err(TransferError::Cancelled) => Outcome::Cancelled,
            Err(TransferError::PublicationUnconfirmed(_)) => Outcome::PublicationUnconfirmed,
            Err(TransferError::Failed(_)) => Outcome::Failed,
        };
        progress
            .results
            .record_with_error(&tree.source, outcome, result.as_ref().err());
        totals.completed += usize::from(published);
        result?;
    }
    Ok(())
}

fn verify_destination(parent: &Parent, directory: &Location) -> Result<(), TransferError> {
    if !parent.matches()? || canonical(directory)? != parent.location.path() {
        return Err(ArchiveError::ExtractionDestinationChanged(directory.clone()).into());
    }
    Ok(())
}

fn unpack_tree(
    archive: &mut Extraction<'_>,
    tree: &SelectedTree,
    payload: &Location,
    target: &Location,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let cancel = progress.cancellation();
    for entry in &tree.entries {
        cancel.check()?;
        let descend = |root: &Location| {
            entry
                .relative
                .iter()
                .fold(root.clone(), |dir, part| dir.join(part))
        };
        let member = descend(&tree.source);
        let path = descend(payload);
        if entry.is_dir() {
            progress.item(member.path());
            if let Some(path) = path.local() {
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
                builder.create(path).map_err(|error| {
                    MutationError::local(path, LocalPathAction::CreateDirectory, error)
                })?;
            } else {
                remote(&path)?.mkdir(path.path(), 0o700).map_err(|source| {
                    MutationError::CreateSftp {
                        destination: path.clone(),
                        source,
                    }
                })?;
            }
        } else {
            let file = StagedFile::open(&path, cancel.clone())?;
            unpack_file(archive, entry, &member, &descend(target), file, progress)?;
        }
    }
    Ok(())
}

fn unpack_file(
    archive: &mut Extraction<'_>,
    entry: &SelectedEntry,
    member: &Location,
    destination: &Location,
    mut file: StagedFile,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    archive
        .copy_entry(entry, member.path(), &mut file, progress)
        .map_err(|source| ArchiveError::ExtractEntry {
            entry: member.clone(),
            destination: destination.clone(),
            source,
        })?;
    file.close()
}
