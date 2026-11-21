use super::copy_metadata::{self, Mode as MetadataMode};
use super::location::{validate_name, Location};
use super::mutations::remove_remote_tree;
use super::sftp::remote;
use super::{err, TransferSummary, MAX_TREE_DEPTH};
use crate::conflict::{Answer, Conflict, Decision, EntryDetails};
use crate::fs_ops::TransferKind;
use crate::operations::{Cancellation, CopyReader, Progress, TransferError};
use crate::{archive, fs_ops};
use ssh2::{ErrorCode, FileStat, OpenFlags, OpenType, RenameFlags};
use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, io};

#[derive(Clone)]
pub(super) enum EntryMetadata {
    Local(LocalEntryMetadata),
    Remote(FileStat),
}

#[derive(Clone)]
pub(super) struct LocalEntryMetadata {
    metadata: fs::Metadata,
    #[cfg(windows)]
    identity: Option<crate::windows_file_id::WindowsFileId>,
}

impl LocalEntryMetadata {
    fn at(path: &Path, metadata: fs::Metadata) -> Self {
        #[cfg(not(windows))]
        let _ = path;
        Self {
            metadata,
            #[cfg(windows)]
            identity: crate::windows_file_id::from_path(path).ok().flatten(),
        }
    }

    pub(super) fn from_file(file: &fs::File, metadata: fs::Metadata) -> Self {
        #[cfg(not(windows))]
        let _ = file;
        Self {
            metadata,
            #[cfg(windows)]
            identity: crate::windows_file_id::from_handle(file).ok().flatten(),
        }
    }
}

impl std::ops::Deref for LocalEntryMetadata {
    type Target = fs::Metadata;

    fn deref(&self) -> &Self::Target {
        &self.metadata
    }
}

impl EntryMetadata {
    pub(super) fn same_local_volume(&self, other: &Self) -> Option<bool> {
        if let (Self::Local(left), Self::Local(right)) = (self, other) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                return Some(left.dev() == right.dev());
            }
            #[cfg(windows)]
            {
                return Some(left.identity?.volume == right.identity?.volume);
            }
            #[cfg(not(any(unix, windows)))]
            let _ = (left, right);
        }
        None
    }

    pub(super) fn is_file(&self) -> bool {
        match self {
            Self::Local(meta) => meta.is_file(),
            Self::Remote(stat) => stat.is_file(),
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn is_directory(&self) -> bool {
        match self {
            Self::Local(meta) => meta.is_dir(),
            Self::Remote(stat) => stat.is_dir(),
        }
    }

    pub(super) fn same_object(&self, other: &Self) -> bool {
        #[cfg(unix)]
        if let (Self::Local(left), Self::Local(right)) = (self, other) {
            use std::os::unix::fs::MetadataExt;
            return left.dev() == right.dev() && left.ino() == right.ino();
        }
        #[cfg(windows)]
        if let (Self::Local(left), Self::Local(right)) = (self, other) {
            return left.identity.is_some() && left.identity == right.identity;
        }
        let _ = other;
        false
    }

    pub(super) fn matches_local_move(&self, current: &Self) -> bool {
        match (self, current) {
            (Self::Local(expected), Self::Local(actual)) => {
                // Renaming one hard-link path changes ctime for all its aliases.
                self.same_object(current)
                    && expected.file_type() == actual.file_type()
                    && expected.created().ok() == actual.created().ok()
            }
            _ => false,
        }
    }

    // Access time can change just by opening a file. Compare the fields
    // relevant to replacement, including the local inode where available.
    pub(super) fn matches_file(&self, current: &Self) -> bool {
        match (self, current) {
            (Self::Local(expected), Self::Local(actual)) => {
                #[cfg(any(unix, windows))]
                if !self.same_object(current) {
                    return false;
                }
                actual.is_file()
                    && expected.len() == actual.len()
                    && expected.modified().ok() == actual.modified().ok()
                    && expected.created().ok() == actual.created().ok()
                    && expected.permissions() == actual.permissions()
            }
            (Self::Remote(expected), Self::Remote(actual)) => {
                actual.is_file()
                    && expected.size == actual.size
                    && expected.mtime == actual.mtime
                    && expected.perm == actual.perm
                    && expected.uid == actual.uid
                    && expected.gid == actual.gid
            }
            _ => false,
        }
    }

    pub(super) fn details(&self, location: &Location) -> EntryDetails {
        let (kind, size, modified) = match self {
            Self::Local(meta) => (
                entry_kind(meta.is_file(), meta.is_dir(), meta.is_symlink()),
                (!meta.is_dir()).then_some(meta.len()),
                meta.modified().ok(),
            ),
            Self::Remote(stat) => (
                entry_kind(stat.is_file(), stat.is_dir(), stat.file_type().is_symlink()),
                stat.size.filter(|_| !stat.is_dir()),
                stat.mtime
                    .and_then(|time| UNIX_EPOCH.checked_add(Duration::from_secs(time))),
            ),
        };
        EntryDetails {
            location: location.clone(),
            kind,
            size,
            modified,
        }
    }
}

pub(super) fn entry_kind(file: bool, dir: bool, symlink: bool) -> crate::i18n::MessageKey {
    if file {
        crate::i18n::key!("Файл")
    } else if dir {
        crate::i18n::key!("Папка")
    } else if symlink {
        crate::i18n::key!("Ссылка")
    } else {
        crate::i18n::key!("Специальный объект")
    }
}

pub(super) fn entry_metadata(location: &Location) -> Result<Option<EntryMetadata>, String> {
    if let Some(path) = location.local() {
        match fs::symlink_metadata(path) {
            Ok(meta) => Ok(Some(EntryMetadata::Local(LocalEntryMetadata::at(
                path, meta,
            )))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{location}: {e}")),
        }
    } else {
        match remote(location)?.lstat(location.path()) {
            Ok(stat) => Ok(Some(EntryMetadata::Remote(stat))),
            Err(e) if e.code() == ErrorCode::SFTP(2) => Ok(None),
            Err(e) => Err(format!("{location}: {e}")),
        }
    }
}

pub(super) struct TransferItem {
    pub(super) original: Location,
    pub(super) source: Location,
    pub(super) target: Location,
    pub(super) covered: Vec<PathBuf>,
    pub(super) local_move_source: Option<EntryMetadata>,
    pub(super) replacement: Option<(EntryMetadata, EntryMetadata)>,
}

impl TransferItem {
    pub(super) fn new(original: Location, source: Location, target: Location) -> Self {
        Self {
            original,
            source,
            target,
            covered: Vec::new(),
            local_move_source: None,
            replacement: None,
        }
    }
}

pub(super) fn transfer_plan(
    sources: &[Location],
    destination: &Location,
    kind: TransferKind,
    progress: &mut Progress,
) -> Result<Vec<TransferItem>, TransferError> {
    if destination.is_archive() || sources.iter().any(Location::is_archive) {
        return Err(archive::readonly().into());
    }
    if let Some(dest) = destination.local()
        && sources.iter().all(|p| !p.remote())
    {
        let paths = sources.iter().map(|p| p.path().into()).collect::<Vec<_>>();
        let guard_source = cfg!(target_os = "macos") && kind == TransferKind::Move;
        let plan = fs_ops::plan_transfer(&paths, dest, progress)?;
        for item in &plan {
            progress.checkpoint()?;
            if kind == TransferKind::Move && item.source == item.target {
                return Err(crate::i18n::text!("Элементы уже находятся в этой папке").into());
            }
        }
        return Ok(plan
            .into_iter()
            .map(|item| {
                let local_move_source = guard_source.then(|| {
                    EntryMetadata::Local(LocalEntryMetadata::at(&item.source, item.source_metadata))
                });
                TransferItem {
                    original: item.original.into(),
                    source: item.source.into(),
                    target: item.target.into(),
                    covered: item.covered,
                    local_move_source,
                    replacement: None,
                }
            })
            .collect());
    }

    let mut names = HashSet::new();
    let mut plan = Vec::new();
    for source in sources {
        progress.checkpoint()?;
        progress.item(source.path());
        let name = source
            .path()
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or(crate::i18n::text!(
                "Нельзя переносить корень или имя не UTF-8"
            ))?;
        validate_name(name)?;
        if !names.insert(name.to_owned()) {
            return Err(crate::i18n::text!("Выбраны элементы с одинаковыми именами").into());
        }
        if source.might_share_filesystem(destination) {
            // Resolve aliases for the destination and the source subtree.
            let actual_source = canonical(source)?;
            let actual_dest = canonical(destination)?;
            if kind == TransferKind::Move {
                let parent = source
                    .parent()
                    .ok_or(crate::i18n::text!("Недопустимый источник"))?;
                if canonical(&parent)? == actual_dest {
                    return Err(crate::i18n::text!("Элементы уже находятся в этой папке").into());
                }
            }
            if actual_dest.starts_with(&actual_source) {
                return Err(crate::i18n::text!(
                    "Нельзя копировать или перемещать элемент в себя или вложенную папку"
                )
                .into());
            }
        }
        plan.push(TransferItem::new(
            source.clone(),
            source.clone(),
            destination.join(name),
        ));
    }
    Ok(plan)
}

pub(super) fn keep_both_name(
    target: &Location,
    reserved: &HashSet<Location>,
    cancel: &Cancellation,
) -> Result<Location, TransferError> {
    let parent = target
        .parent()
        .ok_or(crate::i18n::text!("Недопустимое назначение"))?;
    let stem = target
        .path()
        .file_stem()
        .ok_or(crate::i18n::text!("Нет имени объекта"))?;
    for index in 2..=10_000 {
        cancel.check()?;
        let mut name = stem.to_os_string();
        name.push(format!(" ({index})"));
        if let Some(ext) = target.path().extension() {
            name.push(".");
            name.push(ext);
        }
        let candidate = parent.join(name);
        if !reserved.contains(&candidate) && entry_metadata(&candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    Err(crate::tr_format!(
        "Could not choose an unused name for {target}",
        target = target
    )
    .into())
}

pub fn transfer_with_conflicts(
    sources: &[Location],
    destination: &Location,
    kind: TransferKind,
    progress: &mut Progress,
    decide: impl FnMut(Conflict) -> Result<Answer, String>,
    moved: impl FnMut(&Location, &[PathBuf]),
) -> Result<TransferSummary, String> {
    progress.results.plan(sources.iter().map(|source| {
        let target = source
            .path()
            .file_name()
            .map(|name| destination.join(name).display());
        (source.clone(), target)
    }));
    let mut summary = TransferSummary {
        remaining: sources.to_vec(),
        preparing: true,
        ..TransferSummary::default()
    };
    match transfer_run(
        sources,
        destination,
        kind,
        progress,
        decide,
        moved,
        &mut summary,
    ) {
        Ok(()) => Ok(summary),
        Err(TransferError::Cancelled) => {
            progress.results.cancelled = true;
            summary.cancelled = true;
            Ok(summary)
        }
        Err(TransferError::PublicationUnconfirmed(error)) => {
            let (completed, skipped, remaining) =
                (summary.completed, summary.skipped, summary.remaining.len());
            Err(crate::tr_format!(
                "{error}. Server publication is unconfirmed. Completed: {completed}. Skipped: {skipped}. Remaining: {remaining}. Inspect the source and destination before retrying.", completed = completed, error = error, remaining = remaining, skipped = skipped
            ))
        }
        Err(TransferError::Failed(error)) if summary.preparing => Err(error),
        Err(TransferError::Failed(error)) => {
            let (completed, skipped, remaining) =
                (summary.completed, summary.skipped, summary.remaining.len());
            let paths = summary
                .remaining
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            Err(crate::tr_format!(
                "{error}. Completed: {completed}. Skipped: {skipped}. Remaining: {remaining}: {paths}. Refresh both folders before retrying; check any recovery instructions.", completed = completed, error = error, paths = paths, remaining = remaining, skipped = skipped
            ))
        }
    }
}

pub(super) fn transfer_run(
    sources: &[Location],
    destination: &Location,
    kind: TransferKind,
    progress: &mut Progress,
    mut decide: impl FnMut(Conflict) -> Result<Answer, String>,
    mut moved: impl FnMut(&Location, &[PathBuf]),
    summary: &mut TransferSummary,
) -> Result<(), TransferError> {
    progress.checkpoint()?;
    let mut plan = transfer_plan(sources, destination, kind, progress)?;
    if let Some(retry) = &progress.retry {
        for item in &mut plan {
            retry.use_previous_target(item);
        }
    }
    progress.results.plan(
        plan.iter()
            .map(|item| (item.original.clone(), Some(item.target.display()))),
    );
    let retry_cancel = progress.cancellation().pausable_scan();
    if let Some(retry) = &mut progress.retry {
        retry.prepare(&plan, &retry_cancel)?;
    }
    summary.remaining = plan.iter().map(|item| item.original.clone()).collect();
    summary.preparing = false;
    let cancel = progress.cancellation();
    let mut reserved: HashSet<_> = plan.iter().map(|item| item.target.clone()).collect();
    let targets = plan
        .iter()
        .map(|item| {
            progress.checkpoint()?;
            progress.item(item.original.path());
            if kind == TransferKind::Move && item.source == item.target {
                return Err(crate::i18n::text!("Элементы уже находятся в этой папке").into());
            }
            entry_metadata(&item.target).map_err(Into::into)
        })
        .collect::<Result<Vec<_>, TransferError>>()?;
    let count = targets.iter().flatten().count();
    let mut index = 0;
    let mut group = None;
    let mut ready = Vec::with_capacity(plan.len());
    for (mut item, target_meta) in plan.into_iter().zip(targets) {
        progress.checkpoint()?;
        progress.results.begin(&item.original);
        if let Some(target_meta) = target_meta {
            progress.item(item.original.path());
            index += 1;
            let source_meta = entry_metadata(&item.source)?
                .ok_or_else(|| crate::tr_format!("Source item {} disappeared", item.source))?;
            let can_replace = source_meta.is_file()
                && target_meta.is_file()
                && !source_meta.same_object(&target_meta)
                && (!item.source.same_backend(&item.target)
                    || canonical(&item.source)? != canonical(&item.target)?);
            #[cfg(target_os = "macos")]
            let can_replace = can_replace && !mounted_smb(&item.target);
            #[cfg(windows)]
            let can_replace = can_replace
                && !matches!((item.source.local(), item.target.local()),
                    (Some(source), Some(target)) if fs_ops::same_windows_file(source, target));
            let decision = match group {
                Some(Decision::Replace) if !can_replace => None,
                saved => saved,
            };
            let decision = if let Some(decision) = decision {
                decision
            } else {
                progress.waiting_for_conflict(item.source.path());
                let answer = decide(Conflict {
                    source: source_meta.details(&item.original),
                    target: target_meta.details(&item.target),
                    can_replace,
                    index,
                    count,
                })?;
                // A simultaneous Replace must not override a Stop request.
                cancel.check()?;
                progress.item(item.source.path());
                if answer.apply_to_group {
                    group = Some(answer.decision);
                }
                answer.decision
            };
            match decision {
                Decision::Cancel => return Err(TransferError::Cancelled),
                Decision::Skip => {
                    progress.results.skip_selected(
                        &item.original,
                        item.target.display(),
                        &mut item.covered,
                    );
                    summary.skipped += 1;
                    remove_pending(summary, &item.original);
                    continue;
                }
                Decision::KeepBoth => {
                    item.target = keep_both_name(&item.target, &reserved, &cancel)?;
                    reserved.insert(item.target.clone());
                }
                Decision::Replace if can_replace => {
                    item.replacement = Some((source_meta, target_meta))
                }
                Decision::Replace => {
                    #[cfg(target_os = "macos")]
                    if mounted_smb(&item.target) {
                        return Err(crate::i18n::text(crate::smb::REPLACE_UNAVAILABLE).into());
                    }
                    return Err(crate::i18n::text!(
                        "Замена доступна только для двух разных обычных файлов"
                    )
                    .into());
                }
            }
        }
        progress
            .results
            .target(&item.original, item.target.display());
        ready.push(item);
        // A resolved conflict has not started the filesystem mutation.
        progress.results.clear_active();
    }
    for item in ready {
        #[cfg(target_os = "macos")]
        {
            progress.partial_copy_retained = false;
        }
        progress.checkpoint()?;
        progress.results.begin(&item.original);
        progress.item(item.source.path());
        if let Some(retry) = &progress.retry {
            retry.verify(&item, &cancel)?;
        }
        let expected = item.replacement.as_ref().map(|(_, target)| target);
        if let Some((expected_source, _)) = &item.replacement
            && !entry_metadata(&item.source)?
                .is_some_and(|meta| expected_source.matches_file(&meta))
        {
            return Err(crate::tr_format!(
                "Source file {} changed after the dialog was shown",
                item.source
            )
            .into());
        }
        progress.checkpoint()?;
        let copy_move = kind == TransferKind::Move && super::cross_move::required(&item)?;
        let mut history_warning = None;
        let mut move_manifest = if kind == TransferKind::Move && !copy_move {
            let scan = cancel.clone().pausable_scan();
            let captured = TransferSourceManifest::capture_move_observed(
                &item.source,
                &scan,
                &mut |source, error| {
                    if let Some(error) = error {
                        progress.results.staged(
                            source,
                            Some(error.to_string()),
                            matches!(error, TransferError::Cancelled),
                        );
                    } else {
                        progress
                            .results
                            .capturing(&item.source, &item.target, source);
                    }
                },
            );
            match captured {
                Ok(manifest) => {
                    progress.results.captured();
                    match manifest.verify(&cancel, |source, error| {
                        progress
                            .results
                            .staged(source, Some(error.to_string()), false)
                    }) {
                        Ok(()) => Some(manifest),
                        Err(TransferError::Cancelled) => return Err(TransferError::Cancelled),
                        Err(error) => {
                            progress.results.abandon_group();
                            history_warning = Some(error.to_string());
                            None
                        }
                    }
                }
                Err(TransferError::Cancelled) => return Err(TransferError::Cancelled),
                Err(error) => {
                    progress.results.abandon_group();
                    history_warning = Some(error.to_string());
                    None
                }
            }
        } else {
            None
        };
        let undo_capture = (kind == TransferKind::Move && !copy_move && expected.is_none())
            .then(|| super::undo::Capture::before(&item.source, &item.target).ok())
            .flatten();
        let mut committed = false;
        let mut source_removed = false;
        let published = || {
            committed = true;
            if kind == TransferKind::Move && !copy_move {
                moved(&item.original, &item.covered);
            }
        };
        if kind == TransferKind::Move
            && let Some(retry) = &progress.retry
        {
            retry.verify(&item, &cancel)?;
        }
        if let Some(retry) = &mut progress.retry {
            retry.begin_attempt(&item, kind == TransferKind::Copy || copy_move);
        }
        let mut result = match kind {
            TransferKind::Copy => {
                copy_item_cancellable(&item, expected, progress, MetadataMode::Final, published)
            }
            TransferKind::Move if copy_move => super::cross_move::run(
                &item,
                expected,
                progress,
                published,
                || source_removed = true,
                &mut |_, _| Ok(()),
            ),
            TransferKind::Move => move_item(&item, expected, count != 0, progress, published),
        };
        if copy_move && source_removed {
            moved(&item.original, &item.covered);
        }
        if !committed
            && !copy_move
            && matches!(&result, Err(TransferError::PublicationUnconfirmed(_)))
            && let Some(retry) = &mut progress.retry
        {
            retry.note_source_manifest(&item.original, move_manifest.take());
        }
        let undo_entry = if committed
            && result.is_ok()
            && let (Some(capture), Some(manifest)) = (undo_capture, move_manifest.as_ref())
        {
            capture.finish(&item.source, manifest).ok()
        } else {
            None
        };
        if committed && let Some(manifest) = move_manifest {
            let mut failed = 0usize;
            for (source, entry) in manifest.entries {
                let Ok(relative) = source.path().strip_prefix(item.source.path()) else {
                    continue;
                };
                if relative.as_os_str().is_empty() {
                    continue;
                }
                let target = item.target.join(relative);
                let check = entry_metadata(&target);
                let verified =
                    matches!(&check, Ok(Some(actual)) if entry.matches_moved_target(actual));
                if !verified {
                    failed += 1;
                }
                progress.results.moved_entry(
                    &source,
                    verified,
                    (!verified).then(|| match check {
                        Err(error) => error,
                        _ => crate::tr_format!(
                            "Could not confirm the move of {source} to {target}",
                            source = source,
                            target = target
                        ),
                    }),
                );
            }
            if failed != 0 && result.is_ok() {
                result = Err(crate::tr_format!(
                    "The root move was published, but {failed} descendant results could not be confirmed", failed = failed
                ).into());
            }
        }
        // A published copy never becomes automatically retryable. A move
        // counts as completed only when its source removal was confirmed.
        let outcome = match (&result, committed) {
            (_, true) if copy_move && !source_removed => {
                crate::result_history::Outcome::CopiedSourceRemaining
            }
            (Ok(()), true) => crate::result_history::Outcome::Completed,
            (Err(_), true) => crate::result_history::Outcome::CompletedWithWarning,
            (Err(TransferError::Cancelled), false) => crate::result_history::Outcome::Cancelled,
            #[cfg(target_os = "macos")]
            (Err(_), false) if progress.partial_copy_retained => {
                crate::result_history::Outcome::PartialCopyRetained
            }
            (Err(TransferError::PublicationUnconfirmed(_)), false) => {
                crate::result_history::Outcome::PublicationUnconfirmed
            }
            _ => crate::result_history::Outcome::Failed,
        };
        let metadata_warning = committed && progress.results.active_metadata_warning();
        let outcome = if (history_warning.is_some() || metadata_warning)
            && outcome == crate::result_history::Outcome::Completed
        {
            crate::result_history::Outcome::CompletedWithWarning
        } else {
            outcome
        };
        let mut reason = result.as_ref().err().map(ToString::to_string).or_else(|| {
            history_warning.map(|error| {
                crate::tr_format!(
                    "Move completed, but per-item history is incomplete: {error}",
                    error = error
                )
            })
        });
        if metadata_warning {
            let warning = crate::i18n::text!(
                "Даты или права доступа сохранены частично. Подробности указаны в строках журнала.",
            );
            reason = Some(
                reason.map_or_else(|| warning.into(), |reason| format!("{reason}\n{warning}")),
            );
            summary.metadata_warnings += 1;
        }
        if outcome == crate::result_history::Outcome::Completed
            && let Some(entry) = undo_entry
        {
            progress.undo.push(entry);
        }
        progress
            .results
            .record(&item.original, Some(item.target.display()), outcome, reason);
        record_item_result(
            summary,
            &item.original,
            committed && (!copy_move || source_removed),
            result,
        )?;
    }
    // Do not observe a late request after the final successful publication.
    Ok(())
}

pub(super) fn record_item_result(
    summary: &mut TransferSummary,
    original: &Location,
    committed: bool,
    result: Result<(), TransferError>,
) -> Result<(), TransferError> {
    if committed {
        summary.completed += 1;
        remove_pending(summary, original);
    }
    result
}

pub(super) fn remove_pending(summary: &mut TransferSummary, original: &Location) {
    if let Some(index) = summary.remaining.iter().position(|path| path == original) {
        summary.remaining.remove(index);
    }
}

#[cfg(test)]
pub fn transfer(
    sources: &[Location],
    destination: &Location,
    kind: TransferKind,
) -> Result<usize, String> {
    transfer_with_conflicts(
        sources,
        destination,
        kind,
        &mut Progress::default(),
        |conflict| {
            Err(crate::tr_format!(
                "“{}” already exists. Overwriting is disabled.",
                conflict.target.location
            ))
        },
        |_, _| {},
    )
    .map(|report| report.completed)
}

pub(super) fn canonical(location: &Location) -> Result<PathBuf, String> {
    if let Some(path) = location.local() {
        fs::canonicalize(path).map_err(err)
    } else {
        remote(location)?.realpath(location.path()).map_err(err)
    }
}

#[cfg(test)]
pub(super) fn copy_item(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    progress: &mut Progress,
) -> Result<(), String> {
    copy_item_cancellable(item, expected, progress, MetadataMode::Final, || {})
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
pub(crate) fn copy_sftp_for_drop(
    source: &Location,
    destination: &Path,
    directory: bool,
    progress: &mut Progress,
) -> Result<String, String> {
    use crate::{i18n::text, result_history::Outcome};

    let target = Location::Local(destination.to_owned());
    progress
        .results
        .plan([(source.clone(), Some(target.display()))]);
    progress.results.begin(source);
    let mut committed = false;
    let result = (|| {
        progress.checkpoint()?;
        if !source.remote() || !destination.is_absolute() || destination.file_name().is_none() {
            return Err(text!("Недопустимый путь файла").into());
        }
        if !entry_metadata(source)?.is_some_and(|metadata| {
            if directory {
                metadata.is_directory()
            } else {
                metadata.is_file()
            }
        }) {
            return Err(changed_source(source).into());
        }
        if entry_metadata(&target)?.is_some() {
            return Err(crate::tr_format!(
                "The destination path is already occupied: {target}",
                target = target
            )
            .into());
        }
        let item = TransferItem::new(source.clone(), source.clone(), target);
        copy_item_cancellable(&item, None, progress, MetadataMode::Final, || {
            committed = true
        })
    })();
    let warning = committed && (result.is_err() || progress.results.active_metadata_warning());
    let outcome = match (&result, committed) {
        (_, true) if warning => Outcome::CompletedWithWarning,
        (_, true) => Outcome::Completed,
        (Err(TransferError::Cancelled), false) => {
            progress.results.cancelled = true;
            Outcome::Cancelled
        }
        (Err(TransferError::PublicationUnconfirmed(_)), false) => Outcome::PublicationUnconfirmed,
        (_, false) if progress.partial_copy_retained => Outcome::PartialCopyRetained,
        _ => Outcome::Failed,
    };
    let reason = result.as_ref().err().map(ToString::to_string).or_else(|| {
        warning.then(|| {
            text!(
                "Даты или права доступа сохранены частично. Подробности указаны в строках журнала.",
            )
            .into()
        })
    });
    progress.results.record(source, None, outcome, reason);
    if committed {
        Ok(if warning {
            text!("Файл передан. Подробности о предупреждениях в журнале результатов.")
        } else {
            text!("Файлы переданы принимающему приложению")
        }
        .into())
    } else {
        result
            .map(|()| text!("Файлы переданы принимающему приложению").into())
            .map_err(|error| error.to_string())
    }
}

pub(super) fn copy_item_cancellable(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    progress: &mut Progress,
    metadata: MetadataMode,
    published: impl FnOnce(),
) -> Result<(), TransferError> {
    progress.set_copy_endpoints(&item.source, &item.target);
    #[cfg(target_os = "macos")]
    if mounted_smb(&item.target) {
        if expected.is_some() {
            return Err(crate::i18n::text(crate::smb::REPLACE_UNAVAILABLE).into());
        }
        return copy_to_smb(item, progress, published);
    }
    #[cfg(target_os = "macos")]
    if let Some(directory) = progress.resume_directory.clone()
        && let Some(metadata) = entry_metadata(&item.source)?
    {
        if metadata.is_file() {
            return super::resume_copy::copy(item, expected, &directory, progress, published);
        }
        if metadata.is_directory() && expected.is_none() {
            return super::resume_copy::copy_folder(item, &directory, progress, published);
        }
    }
    if let (Some(source), Some(target)) = (item.source.local(), item.target.local()) {
        return fs_ops::copy_atomically_cancellable(
            source,
            target,
            progress,
            |payload, _| {
                publish_item(
                    &payload.to_path_buf().into(),
                    &item.target,
                    expected,
                    published,
                )
            },
            &mut |_| Ok(()),
        );
    }
    copy_transfer(
        item,
        expected,
        progress,
        &mut |_, _| Ok(()),
        metadata,
        published,
    )
}

#[cfg(target_os = "macos")]
pub(super) fn mounted_smb(location: &Location) -> bool {
    location.local().is_some_and(crate::smb::is_smb_path)
}

#[cfg(target_os = "macos")]
fn copy_to_smb(
    item: &TransferItem,
    progress: &mut Progress,
    published: impl FnOnce(),
) -> Result<(), TransferError> {
    use std::io::{Seek, SeekFrom};

    let target = &item.target;
    let target_path = target.local().expect("mounted SMB path");
    let cancel = progress.cancellation();
    progress.checkpoint()?;
    let metadata = entry_metadata(&item.source)?.ok_or_else(|| changed_source(&item.source))?;
    if !metadata.is_file() {
        return Err(crate::i18n::text!(
            "В SMB можно скопировать обычные файлы со свободными именами.",
        )
        .into());
    }
    if matches!(&metadata, EntryMetadata::Remote(stat) if stat.size.is_none()) {
        return Err(changed_source(&item.source).into());
    }
    if let Some(retry) = &progress.retry {
        retry.verify_snapshot(item, Some(&metadata))?;
    }
    let source = TransferSourceEntry {
        metadata,
        children: None,
    };
    let size = source.size();
    check_copy_space(size, &item.target, &item.target)?;
    let mut input = SourceReader::open(&item.source)?;
    source.verify_metadata(&item.source, &input.metadata()?)?;
    source.verify(&item.source, &cancel, true)?;
    progress.checkpoint()?;
    let unconfirmed = |error: &dyn std::fmt::Display| {
        TransferError::PublicationUnconfirmed(crate::tr_format!(
            "SMB copy is unconfirmed. Check {target} for an incomplete copy before retrying ({error})", error = error, target = target,
        ))
    };
    let mut file = crate::smb::open_upload(target_path).map_err(|error| match error.kind() {
        io::ErrorKind::AlreadyExists
        | io::ErrorKind::PermissionDenied
        | io::ErrorKind::InvalidInput
        | io::ErrorKind::NotFound
        | io::ErrorKind::Unsupported => TransferError::Failed(err(error)),
        _ => unconfirmed(&error),
    })?;
    let result = (|| -> Result<(), TransferError> {
        let identity = EntryMetadata::Local(LocalEntryMetadata::from_file(&file, file.metadata()?));
        let mut data = CopyReader::new((&mut input).take(size), progress.copy_verification());
        let written = progress.copy(item.source.path(), Some(size), &mut data, &mut file)?;
        crate::smb::sync_upload(&file)?;
        if written != size || file.metadata()?.len() != size {
            return Err(changed_source(&item.source).into());
        }
        if let Some(hash) = data.finish() {
            file.seek(SeekFrom::Start(0))?;
            progress.verify_copy(item.source.path(), size, hash, &mut file)?;
        }
        source.verify_metadata(&item.source, &input.metadata()?)?;
        source.verify(&item.source, &cancel, true)?;
        let warning = copy_metadata::finish_file(
            &item.target,
            &copy_metadata::Attributes::from_entry(&source.metadata),
            &mut SourceReader::Local(file),
        )?;
        progress
            .results
            .note_metadata(&item.source, warning.as_deref());
        if !entry_metadata(&item.target)?.is_some_and(|current| identity.same_object(&current)) {
            return Err(changed_source(&item.target).into());
        }
        Ok(())
    })();
    // The destination was created exclusively. A failed upload can leave a
    // new partial file, and deleting its name could race another SMB client.
    result.map_err(|error| unconfirmed(&error))?;
    published();
    Ok(())
}

pub(super) fn move_item(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    had_conflicts: bool,
    progress: &mut Progress,
    moved: impl FnOnce(),
) -> Result<(), TransferError> {
    // The batch waits for every conflict before moving anything. Use the same
    // metadata as selection grouping, including for items with no conflict.
    if let (true, Some(planned)) = (had_conflicts, &item.local_move_source)
        && !entry_metadata(&item.source)?
            .is_some_and(|current| planned.matches_local_move(&current))
    {
        let source = &item.original;
        return Err(crate::tr_format!(
            "Source item {source} changed while waiting for a conflict decision",
            source = source
        )
        .into());
    }
    progress.checkpoint()?;
    publish_item_typed(
        &item.source,
        &item.target,
        expected,
        |staging| {
            if let Some(retry) = &mut progress.retry {
                retry.note_staging(&item.original, staging);
            }
        },
        moved,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CopyPhase {
    Opened,
    Read,
    Verifying,
    EntryCopied,
}

#[cfg(test)]
pub(super) fn copy_transfer_checked(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    progress: &mut Progress,
    observe: &mut impl FnMut(&Location, CopyPhase) -> Result<(), String>,
) -> Result<(), String> {
    copy_transfer_cancellable(item, expected, progress, observe, || {}).map_err(|e| e.to_string())
}

#[cfg(test)]
pub(super) fn copy_transfer_cancellable(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    progress: &mut Progress,
    observe: &mut impl FnMut(&Location, CopyPhase) -> Result<(), String>,
    published: impl FnOnce(),
) -> Result<(), TransferError> {
    copy_transfer(
        item,
        expected,
        progress,
        observe,
        MetadataMode::Final,
        published,
    )
}

fn copy_transfer(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    progress: &mut Progress,
    observe: &mut impl FnMut(&Location, CopyPhase) -> Result<(), String>,
    metadata: MetadataMode,
    published: impl FnOnce(),
) -> Result<(), TransferError> {
    progress.checkpoint()?;
    let cancel = progress.cancellation();
    let scan = cancel.clone().pausable_scan();
    let manifest =
        TransferSourceManifest::capture_observed(&item.source, &scan, &mut |source, error| {
            if let Some(error) = error {
                progress.results.staged(
                    source,
                    Some(error.to_string()),
                    matches!(error, TransferError::Cancelled),
                );
            } else {
                progress
                    .results
                    .capturing(&item.source, &item.target, source);
            }
        })?;
    progress.results.captured();
    if let Some(retry) = &progress.retry {
        retry.verify_snapshot(
            item,
            manifest
                .entries
                .get(&item.source)
                .map(|entry| &entry.metadata),
        )?;
    }
    progress.checkpoint()?;
    let result = with_staging_typed(&item.target, Some(&cancel), |staging, preserve| {
        if let Some(retry) = &mut progress.retry {
            retry.note_staging(&item.original, staging);
        }
        // Finish the probe handle lifecycle before checking cancellation.
        check_copy_space(manifest.bytes, &item.target, staging)?;
        progress.checkpoint()?;
        let payload = staging.join("payload");
        copy_tree(
            &item.source,
            &payload,
            staging.local().and_then(Path::parent),
            progress,
            &manifest,
            observe,
        )?;
        let preserve_metadata = item.source.remote() || item.target.remote();
        let publication = (|| {
            progress.checkpoint()?;
            if preserve_metadata {
                copy_metadata::stage(&item.source, &payload, &manifest, metadata, progress)?;
            }
            manifest.verify(&cancel, |source, error| {
                progress.results.staged(
                    source,
                    Some(error.to_string()),
                    matches!(error, TransferError::Cancelled),
                )
            })?;
            if let Some(retry) = &progress.retry {
                retry.verify(item, &cancel)?;
            }
            cancel.check()?;
            if let Some(expected) = expected {
                let mut attempted = false;
                publish_replacement_with_backup_rename(
                    &payload,
                    &item.target,
                    staging,
                    expected,
                    preserve,
                    &mut attempted,
                    rename_without_replacing,
                )
                .map_err(|error| publication_error(&item.target, attempted, error))
            } else {
                rename_for_publication(&payload, &item.target)
            }
        })();
        match publication {
            Err(TransferError::PublicationUnconfirmed(error)) => {
                *preserve = true;
                return Err(TransferError::PublicationUnconfirmed(crate::tr_format!(
                    "{error}. Temporary folder retained for inspection at {staging}.",
                    error = error,
                    staging = staging
                )));
            }
            Err(error) => {
                if preserve_metadata && !*preserve {
                    // The staging owner will try its normal cleanup and
                    // report any remaining directory.
                    let _ = copy_metadata::reopen_for_cleanup(&item.source, &payload, &manifest);
                }
                return Err(error);
            }
            Ok(()) => {}
        }
        published();
        Ok(())
    });
    if matches!(&result, Err(TransferError::PublicationUnconfirmed(_)))
        && let Some(retry) = &mut progress.retry
    {
        retry.note_source_manifest(&item.original, Some(manifest));
    }
    result
}

// The old destination remains allocated throughout staging: do not subtract its
// size, even for replacement. Re-run for every item as earlier copies use space.
pub(super) fn check_copy_space(
    required: u64,
    target: &Location,
    staging: &Location,
) -> Result<(), String> {
    let parent = target
        .parent()
        .ok_or(crate::i18n::text!("Недопустимое назначение"))?;
    let available = if let Some(path) = parent.local() {
        fs_ops::available_copy_space(path)?
    } else {
        // OpenSSH rejects fstatvfs on directory handles on some platforms.
        // A private, empty probe uses the destination filesystem without writing
        // payload bytes or opening/truncating an existing user file.
        let sftp = remote(staging)?;
        let probe = staging.join("space-probe");
        let mut handle = sftp
            .open_mode(
                probe.path(),
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                0o600,
                OpenType::File,
            )
            .map_err(err)?;
        let stat = handle.statvfs();
        handle.close().map_err(err)?;
        sftp.unlink(probe.path()).map_err(err)?;
        let stat = stat.map_err(|e| crate::tr_format!(
            "Could not check available SFTP space in {parent}: {e}. The server must support fstatvfs@openssh.com; no data for this item was written", e = e, parent = parent
        ))?;
        u128::from(stat.f_bavail) * u128::from(stat.f_frsize)
    };
    fs_ops::ensure_copy_space(required, available, parent)
}

pub(super) fn rename_without_replacing(source: &Location, target: &Location) -> Result<(), String> {
    if let (Some(source), Some(target)) = (source.local(), target.local()) {
        fs_ops::rename_no_replace(source, target)
            .map_err(|e| fs_ops::rename_error(source, target, e))
    } else {
        remote(source)?
            .rename(source.path(), target.path(), Some(RenameFlags::empty()))
            .map_err(err)
    }
}

pub(super) fn rename_for_publication(
    source: &Location,
    target: &Location,
) -> Result<(), TransferError> {
    if target.remote() {
        // A failed session lookup occurs before any mutation and remains a
        // definite failure. Only an issued rename can lose its server reply.
        remote(source)?
            .rename(source.path(), target.path(), Some(RenameFlags::empty()))
            .map_err(|error| TransferError::PublicationUnconfirmed(err(error)))
    } else {
        rename_without_replacing(source, target).map_err(TransferError::Failed)
    }
}

pub(super) fn publish_item(
    payload: &Location,
    target: &Location,
    expected: Option<&EntryMetadata>,
    published: impl FnOnce(),
) -> Result<(), String> {
    if let Some(expected) = expected {
        with_staging(target, |staging, preserve| {
            publish_replacement(payload, target, staging, expected, preserve)?;
            published();
            Ok(())
        })
    } else {
        rename_without_replacing(payload, target)?;
        published();
        Ok(())
    }
}

fn publication_error(target: &Location, attempted: bool, error: String) -> TransferError {
    if target.remote() && attempted {
        TransferError::PublicationUnconfirmed(error)
    } else {
        TransferError::Failed(error)
    }
}

pub(super) fn publish_item_typed(
    payload: &Location,
    target: &Location,
    expected: Option<&EntryMetadata>,
    planned_backup: impl FnOnce(&Location),
    published: impl FnOnce(),
) -> Result<(), TransferError> {
    if let Some(expected) = expected {
        with_staging_typed(target, None, |staging, preserve| {
            planned_backup(staging);
            let mut attempted = false;
            publish_replacement_with_backup_rename(
                payload,
                target,
                staging,
                expected,
                preserve,
                &mut attempted,
                rename_without_replacing,
            )
            .map_err(|error| publication_error(target, attempted, error))?;
            published();
            Ok(())
        })
    } else {
        rename_for_publication(payload, target)?;
        published();
        Ok(())
    }
}

pub(super) fn publish_replacement(
    payload: &Location,
    target: &Location,
    staging: &Location,
    expected: &EntryMetadata,
    preserve: &mut bool,
) -> Result<(), String> {
    let mut attempted = false;
    publish_replacement_with_backup_rename(
        payload,
        target,
        staging,
        expected,
        preserve,
        &mut attempted,
        rename_without_replacing,
    )
}

pub(super) fn publish_replacement_with_backup_rename(
    payload: &Location,
    target: &Location,
    staging: &Location,
    expected: &EntryMetadata,
    preserve: &mut bool,
    attempted: &mut bool,
    backup_rename: impl FnOnce(&Location, &Location) -> Result<(), String>,
) -> Result<(), String> {
    publish_checked_replacement(
        payload,
        target,
        staging,
        preserve,
        attempted,
        backup_rename,
        |candidate| {
            if entry_metadata(candidate)?.is_some_and(|meta| expected.matches_file(&meta)) {
                Ok(())
            } else {
                Err(crate::tr_format!(
                    "Destination {target} changed after the conflict dialog was shown",
                    target = target
                ))
            }
        },
    )
}

pub(super) fn publish_checked_replacement(
    payload: &Location,
    target: &Location,
    staging: &Location,
    preserve: &mut bool,
    attempted: &mut bool,
    backup_rename: impl FnOnce(&Location, &Location) -> Result<(), String>,
    mut check: impl FnMut(&Location) -> Result<(), String>,
) -> Result<(), String> {
    check(target)?;
    let backup = staging.join("previous");
    // A lost SFTP reply can hide a successful rename. Preserve the staging
    // directory until the old file is either restored or safely replaced.
    *preserve = target.remote();
    *attempted = true;
    backup_rename(target, &backup).map_err(|e| {
        crate::i18n::initial_backup_rename_failed(&e, target, &backup, target.remote())
    })?;
    *preserve = true;
    let result = (|| {
        check(&backup)?;
        if !entry_metadata(payload)?.is_some_and(|meta| meta.is_file()) {
            return Err(crate::tr_format!(
                "Source item {payload} is no longer a regular file",
                payload = payload
            ));
        }
        *attempted = true;
        rename_without_replacing(payload, target)
    })();
    if let Err(error) = result {
        *attempted = true;
        if let Err(rollback) = rename_without_replacing(&backup, target) {
            return Err(crate::i18n::replacement_rollback_failed(
                &error,
                &rollback,
                target,
                &backup,
                target.remote(),
            ));
        }
        *preserve = false;
        return Err(error);
    }
    *preserve = false;
    Ok(())
}

pub(super) fn with_staging(
    target: &Location,
    work: impl FnOnce(&Location, &mut bool) -> Result<(), String>,
) -> Result<(), String> {
    with_staging_typed(target, None, |staging, preserve| {
        work(staging, preserve).map_err(Into::into)
    })
    .map_err(|error| error.to_string())
}

static LOCAL_STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

struct LocalStaging {
    path: PathBuf,
    #[cfg(unix)]
    original: fs::Metadata,
    #[cfg(windows)]
    original_id: crate::windows_file_id::WindowsFileId,
    remove_on_drop: bool,
}

impl LocalStaging {
    fn create(
        parent: &Path,
        prefix: &str,
        mut proposed: impl FnMut(&Location) -> Result<(), TransferError>,
    ) -> Result<Self, TransferError> {
        for _ in 0..100 {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let counter = LOCAL_STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("{prefix}{}-{nonce}-{counter}", std::process::id()));
            let location = Location::Local(path.clone());
            proposed(&location)?;
            let builder = fs::DirBuilder::new();
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = builder;
                builder.mode(0o700);
                builder
            };
            match builder.create(&path) {
                Ok(()) => {
                    let original = fs::symlink_metadata(&path).map_err(err)?;
                    if !original.is_dir() || original.file_type().is_symlink() {
                        return Err("Temporary transfer folder changed before copying".into());
                    }
                    #[cfg(windows)]
                    let original_id = crate::windows_file_id::from_path(&path)
                        .map_err(err)?
                        .ok_or("Temporary transfer folder identity unavailable")?;
                    return Ok(Self {
                        path,
                        #[cfg(unix)]
                        original,
                        #[cfg(windows)]
                        original_id,
                        remove_on_drop: true,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    crate::recovery::abandon_staging(&location).map_err(err)?;
                }
                Err(error) => {
                    return Err(fs_ops::copy_space_error(parent, &error)
                        .unwrap_or_else(|| err(error))
                        .into());
                }
            }
        }
        Err(crate::i18n::text!("Не удалось создать временную папку для копирования").into())
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn same_owned(&self) -> io::Result<bool> {
        let candidate = fs::symlink_metadata(&self.path)?;
        if !candidate.is_dir() || candidate.file_type().is_symlink() {
            return Ok(false);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(self.original.dev() == candidate.dev() && self.original.ino() == candidate.ino())
        }
        #[cfg(windows)]
        {
            Ok(crate::windows_file_id::from_path(&self.path)? == Some(self.original_id))
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(false)
        }
    }

    fn keep(&mut self) {
        self.remove_on_drop = false;
    }

    fn close(mut self) -> io::Result<()> {
        self.remove_on_drop = false;
        if !self.same_owned()? {
            return Err(io::Error::other(
                "Staging directory identity changed; data preserved",
            ));
        }
        fs::remove_dir_all(&self.path)
    }
}

impl Drop for LocalStaging {
    fn drop(&mut self) {
        if self.remove_on_drop && self.same_owned().unwrap_or(false) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
pub(super) fn staging_result(
    result: Result<(), String>,
    cleanup: Result<(), String>,
    staging: &Location,
    target: &Location,
) -> Result<(), String> {
    staging_result_typed(result.map_err(Into::into), cleanup, staging, target)
        .map_err(|error| error.to_string())
}

pub(super) fn with_staging_typed(
    target: &Location,
    cancel: Option<&Cancellation>,
    work: impl FnOnce(&Location, &mut bool) -> Result<(), TransferError>,
) -> Result<(), TransferError> {
    staging_typed(target, cancel, false, None, work)
}

/// Source data may be moved here: never recursively remove it on unwinding.
pub(super) fn with_preserved_staging(
    target: &Location,
    cancel: &Cancellation,
    work: impl FnOnce(&Location, &mut bool) -> Result<(), TransferError>,
) -> Result<(), TransferError> {
    staging_typed(target, Some(cancel), true, None, work)
}

pub(super) type StagingNotice<'a> = dyn FnMut(&Location) -> Result<(), TransferError> + 'a;

pub(super) const SYNC_STAGING_PREFIX: &str = ".pawcommander-two-way-";

#[cfg(target_os = "macos")]
pub(super) fn with_tracked_staging(
    target: &Location,
    cancel: &Cancellation,
    notice: &mut StagingNotice<'_>,
    work: impl FnOnce(&Location, &mut bool) -> Result<(), TransferError>,
) -> Result<(), TransferError> {
    staging_typed(target, Some(cancel), true, Some(notice), work)
}

fn staging_typed(
    target: &Location,
    cancel: Option<&Cancellation>,
    mut preserve: bool,
    mut notice: Option<&mut StagingNotice<'_>>,
    work: impl FnOnce(&Location, &mut bool) -> Result<(), TransferError>,
) -> Result<(), TransferError> {
    if let Some(cancel) = cancel {
        cancel.check()?;
    }
    let parent = target
        .parent()
        .ok_or(crate::i18n::text!("Недопустимое назначение"))?;
    // A tracked job keeps one current location in its own durable record.
    // The generic append-only recovery record is bounded independently.
    let use_operation_marker = notice.is_none();
    let prefix = if !use_operation_marker {
        SYNC_STAGING_PREFIX
    } else if parent.remote() {
        ".pawcommander-sftp-"
    } else {
        ".pawcommander-transfer-"
    };
    let mut proposed = |location: &Location| -> Result<(), TransferError> {
        if let Some(notice) = notice.as_deref_mut() {
            notice(location)?;
        } else {
            crate::recovery::plan_staging(location).map_err(err)?;
        }
        Ok(())
    };
    if let Some(parent) = parent.local() {
        let mut staging = LocalStaging::create(parent, prefix, proposed)?;
        let location = Location::Local(staging.path().into());
        if use_operation_marker && let Err(error) = crate::recovery::register_staging(&location) {
            let cleanup = staging.close().map_err(err);
            return staging_result_typed(Err(err(error).into()), cleanup, &location, target);
        }
        if preserve {
            staging.keep();
        }
        let result = work(&location, &mut preserve);
        if preserve {
            staging.keep();
            return result;
        }
        return staging_result_typed(result, staging.close().map_err(err), &location, target);
    }

    let sftp = remote(target)?;
    if let Some(cancel) = cancel {
        cancel.check()?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = LOCAL_STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
    let staging = parent.join(format!("{prefix}{}-{nonce}-{counter}", std::process::id()));
    proposed(&staging)?;
    match sftp.mkdir(staging.path(), 0o700) {
        Ok(()) => {}
        Err(e) if sftp.lstat(staging.path()).is_err() => return Err(err(e).into()),
        Err(e) => {
            return Err(TransferError::PublicationUnconfirmed(crate::tr_format!(
                "Temporary folder creation at {staging} is unconfirmed: {e}. Inspect the registered location before retrying.", e = e, staging = staging
            )));
        }
    }
    if use_operation_marker && let Err(error) = crate::recovery::register_staging(&staging) {
        let cleanup = remove_remote_tree(
            &sftp,
            &staging,
            MAX_TREE_DEPTH + 1,
            &mut Progress::default(),
            false,
        );
        return staging_result_typed(Err(err(error).into()), cleanup, &staging, target);
    }
    let result = work(&staging, &mut preserve);
    if preserve {
        return result;
    }
    // The private staging directory adds one level above the payload, whose
    // source tree may already use all 128 supported descendant levels.
    let cleanup = remove_remote_tree(
        &sftp,
        &staging,
        MAX_TREE_DEPTH + 1,
        &mut Progress::default(),
        false,
    );
    staging_result_typed(result, cleanup, &staging, target)
}

pub(super) fn staging_result_typed(
    result: Result<(), TransferError>,
    cleanup: Result<(), String>,
    staging: &Location,
    target: &Location,
) -> Result<(), TransferError> {
    match cleanup {
        Ok(()) => result,
        Err(e) => {
            let unconfirmed = matches!(result, Err(TransferError::PublicationUnconfirmed(_)));
            let outcome = result
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| {
                    crate::tr_format!("Item {target} was published", target = target)
                });
            let message = crate::tr_format!(
                "{outcome}. Temporary folder remains at {staging}: {e}",
                e = e,
                outcome = outcome,
                staging = staging
            );
            Err(if unconfirmed {
                TransferError::PublicationUnconfirmed(message)
            } else {
                TransferError::Failed(message)
            })
        }
    }
}

#[cfg(test)]
pub(super) fn staged_copy(source: &Location, target: &Location) -> Result<(), String> {
    copy_item(
        &TransferItem::new(source.clone(), source.clone(), target.clone()),
        None,
        &mut Progress::default(),
    )
}

pub(super) fn changed_source(source: &Location) -> String {
    crate::i18n::source_changed_during_copy(source)
}

pub(super) fn remote_source_matches(expected: &FileStat, actual: &FileStat) -> bool {
    // SFTP v3 exposes seconds, no inode or ctime. None is not evidence that a
    // regular file stayed unchanged: require at least type, size and mtime.
    expected.perm.is_some()
        && actual.perm.is_some()
        && (!expected.is_file() || (expected.size.is_some() && expected.mtime.is_some()))
        && expected.size == actual.size
        && expected.mtime == actual.mtime
        && expected.perm == actual.perm
        && expected.uid == actual.uid
        && expected.gid == actual.gid
}

#[derive(Clone)]
pub(super) struct TransferSourceEntry {
    pub(super) metadata: EntryMetadata,
    pub(super) children: Option<Vec<OsString>>,
}

impl TransferSourceEntry {
    pub(super) fn matches_moved_target(&self, actual: &EntryMetadata) -> bool {
        match (&self.metadata, actual) {
            (EntryMetadata::Local(expected), EntryMetadata::Local(actual)) => {
                self.metadata
                    .same_object(&EntryMetadata::Local(actual.clone()))
                    && expected.file_type() == actual.file_type()
                    && expected.len() == actual.len()
                    && expected.modified().ok() == actual.modified().ok()
            }
            (EntryMetadata::Remote(expected), EntryMetadata::Remote(actual)) => {
                remote_source_matches(expected, actual)
            }
            _ => false,
        }
    }
    pub(super) fn verify_metadata(
        &self,
        source: &Location,
        actual: &EntryMetadata,
    ) -> Result<(), String> {
        let matches = match (&self.metadata, actual) {
            (EntryMetadata::Local(expected), EntryMetadata::Local(actual)) => {
                fs_ops::source_metadata_matches(expected, actual).map_err(err)?
            }
            (EntryMetadata::Remote(expected), EntryMetadata::Remote(actual)) => {
                remote_source_matches(expected, actual)
            }
            _ => false,
        };
        if matches {
            Ok(())
        } else {
            Err(changed_source(source))
        }
    }

    pub(super) fn verify(
        &self,
        source: &Location,
        cancel: &Cancellation,
        moving: bool,
    ) -> Result<(), TransferError> {
        cancel.check()?;
        let actual = entry_metadata(source)?.ok_or_else(|| changed_source(source))?;
        self.verify_metadata(source, &actual)?;
        if let Some(children) = &self.children
            && *children != source_children(source, cancel, moving)?
        {
            return Err(changed_source(source).into());
        }
        Ok(())
    }

    pub(super) fn size(&self) -> u64 {
        match &self.metadata {
            EntryMetadata::Local(meta) => meta.len(),
            EntryMetadata::Remote(stat) => stat.size.expect("validated file size"),
        }
    }
}

pub(super) fn validate_source_child_name(source: &Location, name: &OsStr) -> Result<(), String> {
    let name = name.to_str().ok_or_else(|| {
        crate::tr_format!(
            "SFTP transfer does not support a name outside UTF-8 in {source}",
            source = source
        )
    })?;
    validate_name(name).map_err(|error| {
        crate::tr_format!(
            "Unsupported SFTP transfer name in {source}: {error}",
            error = error,
            source = source
        )
    })
}

pub(super) fn source_children(
    source: &Location,
    cancel: &Cancellation,
    moving: bool,
) -> Result<Vec<OsString>, TransferError> {
    cancel.check()?;
    let mut names = Vec::new();
    if let Some(path) = source.local() {
        for entry in fs::read_dir(path).map_err(err)? {
            cancel.check()?;
            names.push(entry.map_err(err)?.file_name());
        }
    } else {
        for (path, _) in remote(source)?.readdir(source.path()).map_err(err)? {
            cancel.check()?;
            let name = path.strip_prefix(source.path()).map_err(err)?;
            let mut parts = name.components();
            match (parts.next(), parts.next()) {
                (Some(Component::Normal(name)), None) => names.push(name.to_os_string()),
                _ => {
                    return Err(
                        crate::tr_format!("Invalid name for SFTP: {}", path.display()).into(),
                    )
                }
            }
        }
    }
    for name in &names {
        cancel.check()?;
        if !moving || source.remote() {
            validate_source_child_name(source, name)?;
        }
    }
    cancel.check()?;
    names.sort();
    cancel.check()?;
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(
            crate::tr_format!("Duplicate names in source {source}", source = source).into(),
        );
    }
    Ok(names)
}

pub(super) struct TransferSourceManifest {
    pub(super) entries: BTreeMap<Location, TransferSourceEntry>,
    pub(super) bytes: u64,
    mode: ManifestMode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ManifestMode {
    Copy,
    #[cfg(target_os = "macos")]
    NativeCopy,
    Move,
}

pub(super) const MAX_MOVE_JOURNAL_ENTRIES: usize = 100_000;

impl TransferSourceManifest {
    #[cfg(test)]
    pub(super) fn capture(source: &Location) -> Result<Self, TransferError> {
        Self::capture_cancellable(source, &Cancellation::default())
    }

    #[cfg(test)]
    pub(super) fn capture_cancellable(
        source: &Location,
        cancel: &Cancellation,
    ) -> Result<Self, TransferError> {
        Self::capture_observed(source, cancel, &mut |_, _| {})
    }

    fn capture_observed(
        source: &Location,
        cancel: &Cancellation,
        observe: &mut impl FnMut(&Location, Option<&TransferError>),
    ) -> Result<Self, TransferError> {
        Self::capture_for(source, cancel, ManifestMode::Copy, observe)
    }

    pub(super) fn capture_move_observed(
        source: &Location,
        cancel: &Cancellation,
        observe: &mut impl FnMut(&Location, Option<&TransferError>),
    ) -> Result<Self, TransferError> {
        Self::capture_for(source, cancel, ManifestMode::Move, observe)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn capture_copy_tree(
        source: &Location,
        cancel: &Cancellation,
        native_names: bool,
    ) -> Result<Self, TransferError> {
        let mode = if native_names {
            ManifestMode::NativeCopy
        } else {
            ManifestMode::Copy
        };
        Self::capture_for(source, cancel, mode, &mut |_, _| {})
    }

    fn capture_for(
        source: &Location,
        cancel: &Cancellation,
        mode: ManifestMode,
        observe: &mut impl FnMut(&Location, Option<&TransferError>),
    ) -> Result<Self, TransferError> {
        let mut manifest = Self {
            entries: BTreeMap::new(),
            bytes: 0,
            mode,
        };
        manifest.capture_entry(source, 0, cancel, observe)?;
        Ok(manifest)
    }

    pub(super) fn capture_entry(
        &mut self,
        source: &Location,
        depth: usize,
        cancel: &Cancellation,
        observe: &mut impl FnMut(&Location, Option<&TransferError>),
    ) -> Result<(), TransferError> {
        observe(source, None);
        let result = self.capture_entry_inner(source, depth, cancel, observe);
        if let Err(error) = &result {
            observe(source, Some(error));
        }
        result
    }

    fn capture_entry_inner(
        &mut self,
        source: &Location,
        depth: usize,
        cancel: &Cancellation,
        observe: &mut impl FnMut(&Location, Option<&TransferError>),
    ) -> Result<(), TransferError> {
        cancel.check()?;
        if self.mode == ManifestMode::Move && self.entries.len() >= MAX_MOVE_JOURNAL_ENTRIES {
            return Err(crate::i18n::text!(
                "Пофайловый журнал переноса превысил лимит 100000 элементов"
            )
            .into());
        }
        if depth > MAX_TREE_DEPTH {
            return Err(crate::i18n::text!("Слишком большая глубина папок (более 128)").into());
        }
        let metadata = entry_metadata(source)?.ok_or_else(|| changed_source(source))?;
        let (is_dir, is_file) = match &metadata {
            EntryMetadata::Local(meta) => {
                meta.modified().map_err(err)?;
                (meta.is_dir(), meta.is_file())
            }
            EntryMetadata::Remote(stat) => {
                if stat.perm.is_none()
                    || (stat.is_file() && (stat.size.is_none() || stat.mtime.is_none()))
                {
                    return Err(crate::tr_format!("The server did not report the type, size, or modification time of {source}. The source cannot be checked safely", source = source).into());
                }
                (stat.is_dir(), stat.is_file())
            }
        };
        let native_names = self.mode != ManifestMode::Copy;
        if !native_names && !is_dir && !is_file {
            return Err(crate::tr_format!(
                "SFTP copying of links and special files is unavailable: {source}",
                source = source
            )
            .into());
        }
        let children = if is_dir {
            Some(source_children(source, cancel, native_names)?)
        } else {
            None
        };
        let entry = TransferSourceEntry { metadata, children };
        if is_file {
            self.bytes = self
                .bytes
                .checked_add(entry.size())
                .ok_or(crate::i18n::text!(
                    "Слишком большой размер копируемого дерева"
                ))?;
        }
        if let Some(children) = &entry.children {
            for name in children {
                self.capture_entry(&source.join(name), depth + 1, cancel, observe)?;
            }
        }
        entry.verify(source, cancel, native_names)?;
        self.entries.insert(source.clone(), entry);
        Ok(())
    }

    pub(super) fn verify(
        &self,
        cancel: &Cancellation,
        mut failed: impl FnMut(&Location, &TransferError),
    ) -> Result<(), TransferError> {
        // Revisit early files too; parent mtime does not track child contents.
        for (source, entry) in &self.entries {
            if let Err(error) = entry.verify(source, cancel, self.mode != ManifestMode::Copy) {
                failed(source, &error);
                return Err(error);
            }
        }
        Ok(())
    }
}

pub(super) enum SourceReader {
    Local(fs::File),
    Remote(ssh2::File),
}

impl SourceReader {
    pub(super) fn open(source: &Location) -> Result<Self, String> {
        if let Some(path) = source.local() {
            fs_ops::open_regular_source(path)
                .map(Self::Local)
                .map_err(err)
        } else {
            remote(source)?
                .open(source.path())
                .map(Self::Remote)
                .map_err(err)
        }
    }

    pub(super) fn metadata(&mut self) -> Result<EntryMetadata, String> {
        match self {
            Self::Local(input) => input
                .metadata()
                .map(|metadata| {
                    EntryMetadata::Local(LocalEntryMetadata::from_file(input, metadata))
                })
                .map_err(err),
            Self::Remote(input) => input.stat().map(EntryMetadata::Remote).map_err(|e| {
                crate::tr_format!(
                    "Could not verify the open SFTP source file (fstat): {e}",
                    e = e
                )
            }),
        }
    }
}

impl Read for SourceReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Local(input) => input.read(buffer),
            Self::Remote(input) => input.read(buffer),
        }
    }
}

pub(super) fn copy_tree(
    source: &Location,
    target: &Location,
    local_parent: Option<&Path>,
    progress: &mut Progress,
    manifest: &TransferSourceManifest,
    observe: &mut impl FnMut(&Location, CopyPhase) -> Result<(), String>,
) -> Result<(), TransferError> {
    let result = copy_tree_entry(source, target, local_parent, progress, manifest, observe);
    progress.results.staged(
        source,
        result.as_ref().err().map(ToString::to_string),
        matches!(result, Err(TransferError::Cancelled)),
    );
    result
}

pub(super) fn copy_tree_entry(
    source: &Location,
    target: &Location,
    local_parent: Option<&Path>,
    progress: &mut Progress,
    manifest: &TransferSourceManifest,
    observe: &mut impl FnMut(&Location, CopyPhase) -> Result<(), String>,
) -> Result<(), TransferError> {
    progress.checkpoint()?;
    progress.item(source.path());
    let cancel = progress.cancellation();
    let entry = manifest
        .entries
        .get(source)
        .ok_or_else(|| changed_source(source))?;
    entry.verify(source, &cancel, false)?;
    let local_write_error = |e| {
        local_parent
            .and_then(|parent| fs_ops::copy_space_error(parent, &e))
            .unwrap_or_else(|| err(e))
    };
    if let Some(children) = &entry.children {
        if let Some(path) = target.local() {
            let mut directory = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                directory.mode(0o700);
            }
            directory.create(path).map_err(local_write_error)?;
        } else {
            remote(target)?.mkdir(target.path(), 0o700).map_err(err)?;
        }
        for name in children {
            copy_tree(
                &source.join(name),
                &target.join(name),
                local_parent,
                progress,
                manifest,
                observe,
            )?;
        }
        cancel.check()?;
        return observe(source, CopyPhase::EntryCopied).map_err(Into::into);
    }
    cancel.check()?;
    let mut input = SourceReader::open(source)?;
    observe(source, CopyPhase::Opened)?;
    let opened_metadata = input.metadata()?;
    if progress
        .retry
        .as_ref()
        .is_some_and(|retry| retry.checking())
        && matches!(opened_metadata, EntryMetadata::Local(_))
        && !entry.metadata.same_object(&opened_metadata)
    {
        return Err(changed_source(source).into());
    }
    entry.verify_metadata(source, &opened_metadata)?;
    // Bound reads to the captured size: a continuously growing source cannot
    // turn a copy into an unbounded stream. Handle/path checks detect growth.
    let expected_size = entry.size();
    let mut bounded = CopyReader::new(
        (&mut input).take(expected_size),
        progress.copy_verification(),
    );
    cancel.check()?;
    let written = if let Some(path) = target.local() {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(path).map_err(local_write_error)?;
        let written = progress
            .copy(
                source.path(),
                Some(expected_size),
                &mut bounded,
                &mut output,
            )
            .map_err(|error| TransferError::io(error, local_write_error))?;
        cancel.check()?;
        output.sync_all().map_err(local_write_error)?;
        written
    } else {
        let mut output = remote(target)?
            .open_mode(
                target.path(),
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                0o600,
                OpenType::File,
            )
            .map_err(err)?;
        let written = progress
            .copy(
                source.path(),
                Some(expected_size),
                &mut bounded,
                &mut output,
            )
            .map_err(|error| TransferError::io(error, err))?;
        cancel.check()?;
        output.flush().map_err(err)?;
        output.close().map_err(err)?;
        written
    };
    let source_hash = bounded.finish();
    observe(source, CopyPhase::Read)?;
    if written != expected_size {
        return Err(changed_source(source).into());
    }
    entry.verify_metadata(source, &input.metadata()?)?;
    entry.verify(source, &cancel, false)?;
    cancel.check()?;
    if let Some(source_hash) = source_hash {
        let mut copied = if let Some(path) = target.local() {
            SourceReader::Local(fs_ops::open_regular_source(path).map_err(err)?)
        } else {
            SourceReader::Remote(remote(target)?.open(target.path()).map_err(err)?)
        };
        if !copied.metadata()?.is_file() {
            return Err(crate::tr_format!(
                "Could not verify copy of {source}: staged item is no longer a regular file",
                source = source
            )
            .into());
        }
        observe(source, CopyPhase::Verifying)?;
        progress
            .verify_copy(source.path(), expected_size, source_hash, &mut copied)
            .map_err(|error| TransferError::io(error, err))?;
    }
    observe(source, CopyPhase::EntryCopied).map_err(Into::into)
}
