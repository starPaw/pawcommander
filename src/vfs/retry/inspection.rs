use super::{entry_metadata, Attempt, EntryMetadata, Location, RetryTransfer, Source};
use crate::{
    fs_ops::TransferKind,
    i18n::text,
    operations::{Cancellation, Progress, TransferError},
    result_history::Outcome,
    vfs::{compare_folders, ComparisonMode},
};

enum Disposition {
    Retry,
    Observed,
    Review,
}

impl RetryTransfer {
    pub(crate) fn inspect(
        &self,
        progress: &mut Progress,
    ) -> Result<(Option<Self>, Option<Self>), TransferError> {
        let cancel = progress.cancellation();
        progress.results.plan(self.sources.iter().map(|source| {
            (
                source.original.clone(),
                source
                    .attempt
                    .as_ref()
                    .map(|attempt| attempt.target.display()),
            )
        }));
        cancel.check()?;
        // An inaccessible or replaced destination folder cannot authorize a
        // write. A read-only report can still describe paths that are visible.
        let destination_matches = self.target_matches()?;
        let mut ready = Vec::new();
        let mut pending = Vec::new();
        for source in &self.sources {
            cancel.check()?;
            progress.item(source.original.path());
            progress.results.begin(&source.original);
            let (disposition, outcome, reason) =
                inspect_source(source, self.kind, destination_matches, progress, &cancel)?;
            progress
                .results
                .record(&source.original, None, outcome, Some(reason));
            match disposition {
                Disposition::Retry => {
                    let mut ready_source = source.clone();
                    if ready_source.review.take() == Some(Outcome::PublicationUnconfirmed)
                        && let Some(attempt) = &mut ready_source.attempt
                    {
                        attempt.require_payload = attempt.copied && attempt.previous.is_some();
                    }
                    ready.push(ready_source);
                }
                Disposition::Review => {
                    let mut pending_source = source.clone();
                    pending_source.review.get_or_insert(Outcome::Failed);
                    pending.push(pending_source);
                }
                Disposition::Observed => {}
            }
        }
        cancel.check()?;
        Ok((self.with_sources(ready), self.with_sources(pending)))
    }

    fn with_sources(&self, sources: Vec<Source>) -> Option<Self> {
        if sources.is_empty() {
            return None;
        }
        let mut plan = Self {
            sources,
            indices: Default::default(),
            kind: self.kind,
            destination: self.destination.clone(),
            target: self.target.clone(),
            checking: true,
        };
        plan.reindex();
        Some(plan)
    }
}

fn inspect_source(
    source: &Source,
    kind: TransferKind,
    destination_matches: bool,
    progress: &mut Progress,
    cancel: &Cancellation,
) -> Result<(Disposition, Outcome, String), TransferError> {
    let current_source = entry_metadata(&source.resolved)?;
    let may_retry =
        source.replay_allowed && destination_matches && source.matches(current_source.as_ref());
    let Some(attempt) = &source.attempt else {
        return Ok(if may_retry {
            (
                Disposition::Retry,
                Outcome::RetryReady,
                text("Источник проверен. Передача этого объекта не началась.").into(),
            )
        } else {
            (
                Disposition::Review,
                Outcome::Failed,
                text("Источник или папка назначения изменились. Повтор недоступен.").into(),
            )
        });
    };

    let current_target = entry_metadata(&attempt.target)?;
    let mut details = vec![
        report_path(&source.resolved, current_source.is_some()),
        report_path(&attempt.target, current_target.is_some()),
    ];
    let staging = observe_path(attempt.staging.as_ref(), &mut details)?;
    let backup = observe_path(attempt.backup().as_ref(), &mut details)?;
    let held = observe_path(attempt.held_source.as_ref(), &mut details)?;
    let payload = if staging && let Some(payload) = attempt.payload() {
        observe_path(Some(&payload), &mut details)?
    } else {
        false
    };
    let checked = |summary: &'static str| format!("{summary}\n{}", details.join("\n"));
    if source.review == Some(Outcome::CopiedSourceRemaining) {
        return Ok((
            Disposition::Observed,
            Outcome::CopiedSourceRemaining,
            checked(text("Копия была опубликована до обрыва. Проверьте остаток источника перед дальнейшими действиями.")),
        ));
    }

    // A remote replacement can have the same size and timestamp as the old
    // file. The retained payload supplies independent evidence that the
    // final rename did not consume it. A retained backup always blocks replay.
    let needs_payload = source.review == Some(Outcome::PublicationUnconfirmed)
        && attempt.copied
        && attempt.previous.is_some();
    let needs_manifest =
        source.review == Some(Outcome::PublicationUnconfirmed) && is_directory(&source.metadata);
    if may_retry
        && attempt.target_matches(current_target.as_ref())
        && !backup
        && !held
        && (!needs_payload || payload)
        && (!needs_manifest || attempt.source_entries.is_some())
    {
        if let Some(manifest) = &attempt.source_entries {
            manifest.verify(cancel, |_, _| {})?;
        }
        return Ok((
            Disposition::Retry,
            Outcome::RetryReady,
            checked(text(
                "Источник и назначение в ожидаемом состоянии. Можно повторить операцию.",
            )),
        ));
    }

    if attempt.copied
        && source.matches(current_source.as_ref())
        && let (Some(left), Some(right)) = (&current_source, &current_target)
        && copied_contents_match(source, attempt, left, right, progress, cancel)?
    {
        let outcome = if kind == TransferKind::Copy {
            Outcome::CompletedWithWarning
        } else {
            Outcome::CopiedSourceRemaining
        };
        return Ok((
            Disposition::Observed,
            outcome,
            checked(text("Содержимое назначения совпадает с источником. Состояние исходной команды сервера не подтверждено.")),
        ));
    }

    if !attempt.copied
        && current_source.is_none()
        && current_target.is_some()
        && moved_tree_matches(source, attempt, cancel)?
    {
        return Ok((
            Disposition::Observed,
            Outcome::PublicationUnconfirmed,
            checked(text("Источник отсутствует. Состав и метаданные назначения совпадают с записанными перед переносом.")),
        ));
    }
    Ok((
        Disposition::Review,
        Outcome::PublicationUnconfirmed,
        checked(text(
            "Состояние файлов не позволяет подтвердить безопасный повтор.",
        )),
    ))
}

fn report_path(path: &Location, exists: bool) -> String {
    if exists {
        crate::tr_format!("Найдено {path}", "Found {path}")
    } else {
        crate::tr_format!("Отсутствует {path}", "Missing {path}")
    }
}

fn observe_path(path: Option<&Location>, details: &mut Vec<String>) -> Result<bool, String> {
    if let Some(path) = path {
        let exists = entry_metadata(path)?.is_some();
        details.push(report_path(path, exists));
        return Ok(exists);
    }
    Ok(false)
}

fn is_directory(metadata: &EntryMetadata) -> bool {
    match metadata {
        EntryMetadata::Local(metadata) => metadata.is_dir(),
        EntryMetadata::Remote(metadata) => metadata.is_dir(),
    }
}

fn copied_contents_match(
    source: &Source,
    attempt: &Attempt,
    left: &EntryMetadata,
    right: &EntryMetadata,
    progress: &mut Progress,
    cancel: &Cancellation,
) -> Result<bool, TransferError> {
    use crate::vfs::read_scope::ReadScope;
    let result = if left.is_file() && right.is_file() {
        let mut digest = |file: &Location| {
            let parent = file.parent().ok_or("Missing file parent")?;
            let name = file.path().file_name().ok_or("Missing file name")?;
            ReadScope::open(&parent)?.digest(name.as_ref(), progress, cancel)
        };
        digest(&source.resolved)?
            .zip(digest(&attempt.target)?)
            .is_some_and(|(left, right)| left == right)
    } else if is_directory(left) && is_directory(right) {
        let compared = compare_folders(
            &[source.resolved.clone(), attempt.target.clone()],
            ComparisonMode::Contents,
            progress,
        )?;
        compared.differences.is_empty()
    } else {
        false
    };
    cancel.check()?;
    Ok(result && source.matches(entry_metadata(&source.resolved)?.as_ref()))
}

fn moved_tree_matches(
    source: &Source,
    attempt: &Attempt,
    cancel: &Cancellation,
) -> Result<bool, TransferError> {
    let Some(manifest) = &attempt.source_entries else {
        return Ok(false);
    };
    for (original, entry) in &manifest.entries {
        cancel.check()?;
        let relative = original
            .path()
            .strip_prefix(source.resolved.path())
            .map_err(|_| "Invalid move manifest")?;
        let current = if relative.as_os_str().is_empty() {
            attempt.target.clone()
        } else {
            attempt.target.join(relative)
        };
        if !entry_metadata(&current)?.is_some_and(|actual| entry.matches_moved_target(&actual)) {
            return Ok(false);
        }
        if let Some(children) = &entry.children
            && *children != crate::vfs::transfer::source_children(&current, cancel, true)?
        {
            return Ok(false);
        }
    }
    Ok(entry_metadata(&source.resolved)?.is_none())
}
