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
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, io};

pub(super) enum EntryMetadata {
    Local(fs::Metadata),
    Remote(FileStat),
}

impl EntryMetadata {
    pub(super) fn is_file(&self) -> bool {
        match self {
            Self::Local(meta) => meta.is_file(),
            Self::Remote(stat) => stat.is_file(),
        }
    }

    pub(super) fn same_object(&self, other: &Self) -> bool {
        #[cfg(unix)]
        if let (Self::Local(left), Self::Local(right)) = (self, other) {
            use std::os::unix::fs::MetadataExt;
            return left.dev() == right.dev() && left.ino() == right.ino();
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
                #[cfg(unix)]
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

pub(super) fn entry_kind(file: bool, dir: bool, symlink: bool) -> &'static str {
    if file {
        "Файл"
    } else if dir {
        "Папка"
    } else if symlink {
        "Ссылка"
    } else {
        "Специальный объект"
    }
}

pub(super) fn entry_metadata(location: &Location) -> Result<Option<EntryMetadata>, String> {
    if let Some(path) = location.local() {
        match fs::symlink_metadata(path) {
            Ok(meta) => Ok(Some(EntryMetadata::Local(meta))),
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
            progress.check_cancelled()?;
            if kind == TransferKind::Move && item.source == item.target {
                return Err(crate::i18n::text("Элементы уже находятся в этой папке").into());
            }
        }
        return Ok(plan
            .into_iter()
            .map(|item| TransferItem {
                original: item.original.into(),
                source: item.source.into(),
                target: item.target.into(),
                covered: item.covered,
                local_move_source: guard_source
                    .then_some(EntryMetadata::Local(item.source_metadata)),
                replacement: None,
            })
            .collect());
    }

    let mut names = HashSet::new();
    let mut plan = Vec::new();
    for source in sources {
        progress.check_cancelled()?;
        progress.item(source.path());
        let name = source
            .path()
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("Нельзя переносить корень или имя не UTF-8")?;
        validate_name(name)?;
        if !names.insert(name.to_owned()) {
            return Err("Выбраны элементы с одинаковыми именами".into());
        }
        if source.same_backend(destination) {
            // Resolve aliases for the destination and the source subtree.
            let actual_source = canonical(source)?;
            let actual_dest = canonical(destination)?;
            if kind == TransferKind::Move {
                let parent = source.parent().ok_or("Недопустимый источник")?;
                if canonical(&parent)? == actual_dest {
                    return Err(crate::i18n::text("Элементы уже находятся в этой папке").into());
                }
            }
            if actual_dest.starts_with(&actual_source) {
                return Err(
                    "Нельзя копировать или перемещать элемент в себя или вложенную папку".into(),
                );
            }
        }
        if kind == TransferKind::Move && (!source.remote() || !source.same_backend(destination)) {
            return Err("Перемещение между локальной папкой и SFTP или разными серверами недоступно. Скопируйте, проверьте копию и удалите оригинал отдельно.".into());
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
    let parent = target.parent().ok_or("Недопустимое назначение")?;
    let stem = target.path().file_stem().ok_or("Нет имени объекта")?;
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
    Err(format!("Не удалось подобрать свободное имя для {target}").into())
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
                "{error}. Завершено: {completed}. Пропущено: {skipped}. Осталось: {remaining}: {paths}. Обновите обе папки перед повтором; проверьте сообщения о восстановлении.",
                "{error}. Completed: {completed}. Skipped: {skipped}. Remaining: {remaining}: {paths}. Refresh both folders before retrying; check any recovery instructions."
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
    progress.check_cancelled()?;
    let plan = transfer_plan(sources, destination, kind, progress)?;
    progress.results.plan(
        plan.iter()
            .map(|item| (item.original.clone(), Some(item.target.display()))),
    );
    summary.remaining = plan.iter().map(|item| item.original.clone()).collect();
    summary.preparing = false;
    let cancel = progress.cancellation();
    let mut reserved: HashSet<_> = plan.iter().map(|item| item.target.clone()).collect();
    let targets = plan
        .iter()
        .map(|item| {
            cancel.check()?;
            progress.item(item.original.path());
            if kind == TransferKind::Move && item.source == item.target {
                return Err(crate::i18n::text("Элементы уже находятся в этой папке").into());
            }
            entry_metadata(&item.target).map_err(Into::into)
        })
        .collect::<Result<Vec<_>, TransferError>>()?;
    let count = targets.iter().flatten().count();
    let mut index = 0;
    let mut group = None;
    let mut ready = Vec::with_capacity(plan.len());
    for (mut item, target_meta) in plan.into_iter().zip(targets) {
        cancel.check()?;
        progress.results.begin(&item.original);
        if let Some(target_meta) = target_meta {
            progress.item(item.original.path());
            index += 1;
            let source_meta = entry_metadata(&item.source)?
                .ok_or_else(|| format!("Исходный объект {} исчез", item.source))?;
            let can_replace = source_meta.is_file()
                && target_meta.is_file()
                && !source_meta.same_object(&target_meta)
                && (!item.source.same_backend(&item.target)
                    || canonical(&item.source)? != canonical(&item.target)?);
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
                    progress.results.record(
                        &item.original,
                        Some(item.target.display()),
                        crate::result_history::Outcome::Skipped,
                        Some(crate::i18n::text("Пропущено при разрешении конфликта").into()),
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
                    return Err("Замена доступна только для двух разных обычных файлов".into());
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
        cancel.check()?;
        progress.results.begin(&item.original);
        progress.item(item.source.path());
        let expected = item.replacement.as_ref().map(|(_, target)| target);
        if let Some((expected_source, _)) = &item.replacement
            && !entry_metadata(&item.source)?
                .is_some_and(|meta| expected_source.matches_file(&meta))
        {
            return Err(format!(
                "Исходный файл {} изменился после показа диалога",
                item.source
            )
            .into());
        }
        cancel.check()?;
        let mut committed = false;
        let published = || {
            committed = true;
            if kind == TransferKind::Move {
                moved(&item.original, &item.covered);
            }
        };
        let result = match kind {
            TransferKind::Copy => copy_item_cancellable(&item, expected, progress, published),
            TransferKind::Move => move_item(&item, expected, count != 0, &cancel, published),
        };
        // Publication precedes cleanup: a committed item stays completed even
        // when cleanup fails, and must not appear in the retryable remainder.
        let outcome = match (&result, committed) {
            (Ok(()), true) => crate::result_history::Outcome::Completed,
            (Err(_), true) => crate::result_history::Outcome::CompletedWithWarning,
            (Err(TransferError::Cancelled), false) => crate::result_history::Outcome::Cancelled,
            _ => crate::result_history::Outcome::Failed,
        };
        progress.results.record(
            &item.original,
            Some(item.target.display()),
            outcome,
            result.as_ref().err().map(ToString::to_string),
        );
        record_item_result(summary, &item.original, committed, result)?;
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
            Err(format!(
                "«{}» уже существует; перезапись запрещена",
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
    copy_item_cancellable(item, expected, progress, || {}).map_err(|e| e.to_string())
}

pub(super) fn copy_item_cancellable(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    progress: &mut Progress,
    published: impl FnOnce(),
) -> Result<(), TransferError> {
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
    copy_transfer_cancellable(item, expected, progress, &mut |_, _| Ok(()), published)
}

pub(super) fn move_item(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    had_conflicts: bool,
    cancel: &Cancellation,
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
            "Исходный объект {source} изменился во время ожидания решения о конфликте",
            "Source item {source} changed while waiting for a conflict decision"
        )
        .into());
    }
    cancel.check()?;
    publish_item(&item.source, &item.target, expected, moved).map_err(Into::into)
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

pub(super) fn copy_transfer_cancellable(
    item: &TransferItem,
    expected: Option<&EntryMetadata>,
    progress: &mut Progress,
    observe: &mut impl FnMut(&Location, CopyPhase) -> Result<(), String>,
    published: impl FnOnce(),
) -> Result<(), TransferError> {
    progress.check_cancelled()?;
    let cancel = progress.cancellation();
    let manifest =
        TransferSourceManifest::capture_observed(&item.source, &cancel, &mut |source, error| {
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
    cancel.check()?;
    with_staging_typed(&item.target, Some(&cancel), |staging, preserve| {
        // Finish the probe handle lifecycle before checking cancellation.
        check_copy_space(manifest.bytes, &item.target, staging)?;
        cancel.check()?;
        let payload = staging.join("payload");
        copy_tree(
            &item.source,
            &payload,
            staging.local().and_then(Path::parent),
            progress,
            &manifest,
            observe,
        )?;
        manifest.verify(&cancel, |source, error| {
            progress.results.staged(
                source,
                Some(error.to_string()),
                matches!(error, TransferError::Cancelled),
            )
        })?;
        cancel.check()?;
        if let Some(expected) = expected {
            publish_replacement(&payload, &item.target, staging, expected, preserve)?;
        } else {
            rename_without_replacing(&payload, &item.target)?;
        }
        published();
        Ok(())
    })
}

// The old destination remains allocated throughout staging: do not subtract its
// size, even for replacement. Re-run for every item as earlier copies use space.
pub(super) fn check_copy_space(
    required: u64,
    target: &Location,
    staging: &Location,
) -> Result<(), String> {
    let parent = target.parent().ok_or("Недопустимое назначение")?;
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
            "Не удалось проверить свободное место SFTP для {parent}: {e}. Сервер должен поддерживать fstatvfs@openssh.com; данные текущего объекта не записаны",
            "Could not check available SFTP space in {parent}: {e}. The server must support fstatvfs@openssh.com; no data for this item was written"
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

pub(super) fn publish_replacement(
    payload: &Location,
    target: &Location,
    staging: &Location,
    expected: &EntryMetadata,
    preserve: &mut bool,
) -> Result<(), String> {
    publish_replacement_with_backup_rename(
        payload,
        target,
        staging,
        expected,
        preserve,
        rename_without_replacing,
    )
}

pub(super) fn publish_replacement_with_backup_rename(
    payload: &Location,
    target: &Location,
    staging: &Location,
    expected: &EntryMetadata,
    preserve: &mut bool,
    backup_rename: impl FnOnce(&Location, &Location) -> Result<(), String>,
) -> Result<(), String> {
    let target_changed = || {
        crate::tr_format!(
            "Назначение {target} изменилось после показа диалога",
            "Destination {target} changed after the conflict dialog was shown"
        )
    };
    if !entry_metadata(target)?.is_some_and(|meta| expected.matches_file(&meta)) {
        return Err(target_changed());
    }
    let backup = staging.join("previous");
    // A lost SFTP reply can hide a successful rename. Preserve the staging
    // directory until the old file is either restored or safely replaced.
    *preserve = target.remote();
    backup_rename(target, &backup).map_err(|e| {
        crate::i18n::initial_backup_rename_failed(&e, target, &backup, target.remote())
    })?;
    *preserve = true;
    let result = (|| {
        if !entry_metadata(&backup)?.is_some_and(|meta| expected.matches_file(&meta)) {
            return Err(target_changed());
        }
        if !entry_metadata(payload)?.is_some_and(|meta| meta.is_file()) {
            return Err(crate::tr_format!(
                "Исходный объект {payload} больше не является обычным файлом",
                "Source item {payload} is no longer a regular file"
            ));
        }
        rename_without_replacing(payload, target)
    })();
    if let Err(error) = result {
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
    if let Some(cancel) = cancel {
        cancel.check()?;
    }
    let parent = target.parent().ok_or("Недопустимое назначение")?;
    let mut preserve = false;
    if let Some(parent) = parent.local() {
        let mut builder = tempfile::Builder::new();
        builder.prefix(".pawcommander-transfer-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let staging = builder
            .tempdir_in(parent)
            .map_err(|e| fs_ops::copy_space_error(parent, &e).unwrap_or_else(|| err(e)))?;
        let location = Location::Local(staging.path().into());
        let result = work(&location, &mut preserve);
        if preserve {
            let _ = staging.keep();
            return result;
        }
        return staging_result_typed(result, staging.close().map_err(err), &location, target);
    }

    let sftp = remote(target)?;
    let mut staging = None;
    for counter in 0..100 {
        if let Some(cancel) = cancel {
            cancel.check()?;
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let candidate = parent.join(format!(
            ".pawcommander-sftp-{}-{nonce}-{counter}",
            std::process::id()
        ));
        match sftp.mkdir(candidate.path(), 0o700) {
            Ok(()) => {
                staging = Some(candidate);
                break;
            }
            Err(e) if sftp.lstat(candidate.path()).is_err() => return Err(err(e).into()),
            Err(_) => {}
        }
    }
    let staging = staging.ok_or("Не удалось создать временную папку SFTP")?;
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
            let outcome = result
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| {
                    crate::tr_format!("Объект {target} опубликован", "Item {target} was published")
                });
            Err(crate::tr_format!(
                "{outcome}. Осталась временная папка {staging}: {e}",
                "{outcome}. Temporary folder remains at {staging}: {e}"
            )
            .into())
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

pub(super) struct TransferSourceEntry {
    pub(super) metadata: EntryMetadata,
    pub(super) children: Option<Vec<OsString>>,
}

impl TransferSourceEntry {
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
    ) -> Result<(), TransferError> {
        cancel.check()?;
        let actual = entry_metadata(source)?.ok_or_else(|| changed_source(source))?;
        self.verify_metadata(source, &actual)?;
        if let Some(children) = &self.children
            && *children != source_children(source, cancel)?
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
    let name = name
        .to_str()
        .ok_or_else(|| format!("SFTP-передача не поддерживает имя не UTF-8 в {source}"))?;
    validate_name(name)
        .map_err(|error| format!("Неподдерживаемое имя для передачи SFTP в {source}: {error}"))
}

pub(super) fn source_children(
    source: &Location,
    cancel: &Cancellation,
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
                _ => return Err(format!("Недопустимое имя SFTP: {}", path.display()).into()),
            }
        }
    }
    for name in &names {
        cancel.check()?;
        validate_source_child_name(source, name)?;
    }
    cancel.check()?;
    names.sort();
    cancel.check()?;
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(format!("Повторяющиеся имена в источнике {source}").into());
    }
    Ok(names)
}

pub(super) struct TransferSourceManifest {
    pub(super) entries: BTreeMap<Location, TransferSourceEntry>,
    pub(super) bytes: u64,
}

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
        let mut manifest = Self {
            entries: BTreeMap::new(),
            bytes: 0,
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
        if depth > MAX_TREE_DEPTH {
            return Err("Слишком большая глубина папок (более 128)".into());
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
                    return Err(format!("Сервер не сообщил тип, размер или время изменения {source}; безопасная проверка источника недоступна").into());
                }
                (stat.is_dir(), stat.is_file())
            }
        };
        if !is_dir && !is_file {
            return Err(format!(
                "SFTP-копирование ссылок и специальных файлов пока недоступно: {source}"
            )
            .into());
        }
        let children = if is_dir {
            Some(source_children(source, cancel)?)
        } else {
            None
        };
        let entry = TransferSourceEntry { metadata, children };
        if is_file {
            self.bytes = self
                .bytes
                .checked_add(entry.size())
                .ok_or("Слишком большой размер копируемого дерева")?;
        }
        if let Some(children) = &entry.children {
            for name in children {
                self.capture_entry(&source.join(name), depth + 1, cancel, observe)?;
            }
        }
        entry.verify(source, cancel)?;
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
            if let Err(error) = entry.verify(source, cancel) {
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
    pub(super) fn metadata(&mut self) -> Result<EntryMetadata, String> {
        match self {
            Self::Local(input) => input.metadata().map(EntryMetadata::Local).map_err(err),
            Self::Remote(input) => input.stat().map(EntryMetadata::Remote).map_err(|e| {
                format!("Не удалось проверить открытый исходный файл SFTP (fstat): {e}")
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
    progress.check_cancelled()?;
    progress.item(source.path());
    let cancel = progress.cancellation();
    let entry = manifest
        .entries
        .get(source)
        .ok_or_else(|| changed_source(source))?;
    entry.verify(source, &cancel)?;
    let local_write_error = |e| {
        local_parent
            .and_then(|parent| fs_ops::copy_space_error(parent, &e))
            .unwrap_or_else(|| err(e))
    };
    if let Some(children) = &entry.children {
        if let Some(path) = target.local() {
            fs::create_dir(path).map_err(local_write_error)?;
        } else {
            remote(target)?.mkdir(target.path(), 0o755).map_err(err)?;
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
    let mut input = if let Some(path) = source.local() {
        SourceReader::Local(fs_ops::open_regular_source(path).map_err(err)?)
    } else {
        SourceReader::Remote(remote(source)?.open(source.path()).map_err(err)?)
    };
    observe(source, CopyPhase::Opened)?;
    entry.verify_metadata(source, &input.metadata()?)?;
    // Bound reads to the captured size: a continuously growing source cannot
    // turn a copy into an unbounded stream. Handle/path checks detect growth.
    let expected_size = entry.size();
    let mut bounded = CopyReader::new(
        (&mut input).take(expected_size),
        progress.copy_verification(),
    );
    cancel.check()?;
    let written = if let Some(path) = target.local() {
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(local_write_error)?;
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
                0o644,
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
    entry.verify(source, &cancel)?;
    cancel.check()?;
    if let Some(source_hash) = source_hash {
        let mut copied = if let Some(path) = target.local() {
            SourceReader::Local(fs_ops::open_regular_source(path).map_err(err)?)
        } else {
            SourceReader::Remote(remote(target)?.open(target.path()).map_err(err)?)
        };
        if !copied.metadata()?.is_file() {
            return Err(crate::tr_format!(
                "Не удалось проверить копию {source}: временный объект больше не является обычным файлом",
                "Could not verify copy of {source}: staged item is no longer a regular file"
            ).into());
        }
        observe(source, CopyPhase::Verifying)?;
        progress
            .verify_copy(source.path(), expected_size, source_hash, &mut copied)
            .map_err(|error| TransferError::io(error, err))?;
    }
    observe(source, CopyPhase::EntryCopied).map_err(Into::into)
}
