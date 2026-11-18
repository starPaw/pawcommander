use super::{
    err,
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
    archive::{Extraction, SelectedTree},
    conflict::{Answer, Conflict, Decision, EntryDetails},
    fs_ops,
    operations::{Progress, TransferError},
    result_history::Outcome,
};
use std::{collections::HashSet, fs};

#[cfg(test)]
mod tests;

pub(crate) fn extract_archive(
    sources: &[Location],
    directory: &Location,
    progress: &mut Progress,
    decide: impl FnMut(Conflict) -> Result<Answer, String>,
) -> Result<String, String> {
    let mut totals = Totals {
        selected: sources.len(),
        ..Totals::default()
    };
    let result = extract(sources, directory, progress, decide, &mut totals);
    match result {
        Ok(()) => Ok(totals.text()),
        Err(TransferError::Cancelled) => {
            progress.results.cancelled = true;
            let summary = totals.text();
            Ok(crate::tr_format!(
                "Распаковка остановлена. {summary}",
                "Extraction stopped. {summary}"
            ))
        }
        Err(error) if totals.completed + totals.skipped != 0 => {
            Err(format!("{error}. {}", totals.text()))
        }
        Err(error) => Err(error.to_string()),
    }
}

#[derive(Default)]
struct Totals {
    selected: usize,
    completed: usize,
    skipped: usize,
}

impl Totals {
    fn text(&self) -> String {
        let Self {
            selected,
            completed,
            skipped,
        } = *self;
        let remaining = selected - completed - skipped;
        crate::tr_format!(
            "Распаковано: {completed}. Пропущено: {skipped}. Осталось: {remaining}.",
            "Extracted: {completed}. Skipped: {skipped}. Remaining: {remaining}."
        )
    }
}

fn extract(
    sources: &[Location],
    directory: &Location,
    progress: &mut Progress,
    mut decide: impl FnMut(Conflict) -> Result<Answer, String>,
    totals: &mut Totals,
) -> Result<(), TransferError> {
    if directory.is_archive() {
        return Err(crate::archive::readonly().into());
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
    if let Some(path) = sources.first().and_then(Location::archive_source) {
        progress.item(path);
    }
    let (mut archive, trees) = Extraction::prepare(sources, &cancel)?;
    totals.selected = trees.len();
    let parent = Parent::capture(directory)?;
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
        count += usize::from(entry_metadata(target)?.is_some());
    }
    let mut index = 0;
    let mut group = None;
    for (tree, mut target) in trees.into_iter().zip(targets) {
        cancel.check()?;
        progress.item(tree.source.path());
        progress.results.begin(&tree.source);
        verify_destination(&parent, directory)?;
        archive.verify_source()?;
        let mut replace = None;
        if let Some(existing) = entry_metadata(&target)? {
            index += 1;
            let first = &tree.entries[0];
            let can_replace = !first.is_dir()
                && existing.is_file()
                && match &existing {
                    EntryMetadata::Local(metadata) => {
                        !archive.is_source_file(target.path(), metadata)
                    }
                    EntryMetadata::Remote(_) => tree.source.archive_source() != Some(target.path()),
                };
            let decision = if let Some(saved) =
                group.filter(|&saved| saved != Decision::Replace || can_replace)
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
                    can_replace,
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
                    target = keep_both_name(&target, &reserved, &cancel)?;
                    reserved.insert(target.clone());
                }
                Decision::Replace if can_replace => replace = Some(existing),
                Decision::Replace => {
                    return Err(crate::i18n::text(
                        "Замена доступна только для двух разных обычных файлов.",
                    )
                    .into())
                }
            }
        }
        progress.results.target(&tree.source, target.display());
        if let Some(path) = parent.location.local() {
            fs_ops::ensure_copy_space(
                tree.bytes,
                fs_ops::available_copy_space(path)?,
                &parent.location,
            )?;
        }
        let mut published = false;
        let result = with_staging_typed(&target, Some(&cancel), |staging, _| {
            let payload = staging.join("payload");
            unpack_tree(&mut archive, &tree, &payload, progress)?;
            progress.item(target.path());
            archive.verify_source()?;
            verify_destination(&parent, directory)?;
            cancel.check()?;
            publish_item_typed(&payload, &target, replace.as_ref(), || published = true)
        });
        let (outcome, reason) = match &result {
            Ok(()) => (Outcome::Completed, None),
            Err(error) => (
                match error {
                    _ if published => Outcome::CompletedWithWarning,
                    TransferError::Cancelled => Outcome::Cancelled,
                    TransferError::PublicationUnconfirmed(_) => Outcome::PublicationUnconfirmed,
                    TransferError::Failed(_) => Outcome::Failed,
                },
                Some(error.to_string()),
            ),
        };
        progress.results.record(&tree.source, None, outcome, reason);
        totals.completed += usize::from(published);
        result?;
    }
    Ok(())
}

fn verify_destination(parent: &Parent, directory: &Location) -> Result<(), TransferError> {
    if parent.verify().is_err() || canonical(directory)? != parent.location.path() {
        return Err(crate::tr_format!(
            "Папка назначения изменилась: {directory}",
            "The destination folder changed: {directory}"
        )
        .into());
    }
    Ok(())
}

fn unpack_tree(
    archive: &mut Extraction,
    tree: &SelectedTree,
    payload: &Location,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let cancel = progress.cancellation();
    for entry in &tree.entries {
        cancel.check()?;
        let member = tree.source.join(&entry.relative);
        let path = entry
            .relative
            .iter()
            .fold(payload.clone(), |dir, part| dir.join(part));
        let context = |error| format!("{member}: {error}");
        if entry.is_dir() {
            progress.item(member.path());
            if let Some(path) = path.local() {
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
                builder
                    .create(path)
                    .map_err(|error| TransferError::io(error, context))?;
            } else {
                remote(&path)?.mkdir(path.path(), 0o700).map_err(err)?;
            }
        } else {
            let mut file = StagedFile::open(&path, cancel.clone())?;
            archive
                .copy_entry(entry, member.path(), &mut file, progress)
                .map_err(|error| TransferError::io(error, context))?;
            file.close()?;
        }
    }
    Ok(())
}
