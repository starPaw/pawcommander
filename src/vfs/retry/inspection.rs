use super::{entry_metadata, Attempt, EntryMetadata, Location, RetryTransfer, Source};
use crate::{
    file_error::{DescendantProblem, MoveHistoryError, TransferPlanError},
    fs_ops::TransferKind,
    operations::{Cancellation, Progress, TransferError},
    result_history::Outcome,
    vfs::{compare_folders, ComparisonMode},
};

pub(crate) struct InspectionReport {
    pub(crate) finding: InspectionFinding,
    pub(crate) paths: Vec<InspectedPath>,
}

pub(crate) enum InspectionFinding {
    BeforeAttempt(Option<InspectionBlocker>),
    Ready,
    CopyPublished,
    MatchingContents(TransferKind),
    MatchingMove,
    Unconfirmed(InspectionBlocker),
}

pub(crate) enum InspectionBlocker {
    RelatedItem,
    DestinationFolder(Location),
    SourceChanged,
    TargetChanged,
    BackupRetained,
    OriginalRetained,
    PayloadUnconfirmed,
    DirectorySnapshotMissing,
}

pub(crate) enum InspectionRole {
    Source,
    Destination,
    StagingFolder,
    Backup,
    HeldSource,
    Payload,
}

pub(crate) struct InspectedPath {
    pub(crate) role: InspectionRole,
    pub(crate) location: Location,
    pub(crate) present: bool,
}

enum Disposition {
    Retry,
    Observed,
    Review,
}

impl InspectionFinding {
    fn decision(&self) -> (Disposition, Outcome) {
        match self {
            Self::BeforeAttempt(None) | Self::Ready => (Disposition::Retry, Outcome::RetryReady),
            Self::BeforeAttempt(Some(_)) => (Disposition::Review, Outcome::Failed),
            Self::CopyPublished | Self::MatchingContents(TransferKind::Move) => {
                (Disposition::Observed, Outcome::CopiedSourceRemaining)
            }
            Self::MatchingContents(TransferKind::Copy) => {
                (Disposition::Observed, Outcome::CompletedWithWarning)
            }
            Self::MatchingMove => (Disposition::Observed, Outcome::PublicationUnconfirmed),
            Self::Unconfirmed(_) => (Disposition::Review, Outcome::PublicationUnconfirmed),
        }
    }
}

impl RetryTransfer {
    pub(crate) fn inspect(
        &self,
        progress: &mut Progress,
    ) -> Result<(Option<Self>, Option<Self>), TransferError> {
        let cancel = progress.cancellation();
        progress
            .results
            .plan_selected_items_with_targets(self.sources.iter().map(|source| {
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
            let report = inspect_source(
                source,
                self.kind,
                &self.destination,
                destination_matches,
                progress,
                &cancel,
            )?;
            let (disposition, outcome) = report.finding.decision();
            progress
                .results
                .record_inspection(&source.original, outcome, &report);
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
    destination: &Location,
    destination_matches: bool,
    progress: &mut Progress,
    cancel: &Cancellation,
) -> Result<InspectionReport, TransferError> {
    let current_source = entry_metadata(&source.resolved)?;
    let blocker = if !source.replay_allowed {
        Some(InspectionBlocker::RelatedItem)
    } else if !destination_matches {
        Some(InspectionBlocker::DestinationFolder(destination.clone()))
    } else if !source.matches(current_source.as_ref())? {
        Some(InspectionBlocker::SourceChanged)
    } else {
        None
    };
    let mut paths = vec![InspectedPath {
        role: InspectionRole::Source,
        location: source.resolved.clone(),
        present: current_source.is_some(),
    }];
    let Some(attempt) = &source.attempt else {
        return Ok(InspectionReport {
            finding: InspectionFinding::BeforeAttempt(blocker),
            paths,
        });
    };

    let current_target = entry_metadata(&attempt.target)?;
    paths.push(InspectedPath {
        role: InspectionRole::Destination,
        location: attempt.target.clone(),
        present: current_target.is_some(),
    });
    let staging = observe_path(
        InspectionRole::StagingFolder,
        attempt.staging.clone(),
        &mut paths,
    )?;
    let backup = observe_path(InspectionRole::Backup, attempt.backup(), &mut paths)?;
    let held = observe_path(
        InspectionRole::HeldSource,
        attempt.held_source.clone(),
        &mut paths,
    )?;
    let payload = staging && observe_path(InspectionRole::Payload, attempt.payload(), &mut paths)?;
    if source.review == Some(Outcome::CopiedSourceRemaining) {
        return Ok(InspectionReport {
            finding: InspectionFinding::CopyPublished,
            paths,
        });
    }

    // A remote replacement can have the same size and timestamp as the old
    // file. The retained payload supplies independent evidence that the
    // final rename did not consume it. A retained backup always blocks replay.
    let needs_payload = source.review == Some(Outcome::PublicationUnconfirmed)
        && attempt.copied
        && attempt.previous.is_some();
    let needs_manifest =
        source.review == Some(Outcome::PublicationUnconfirmed) && is_directory(&source.metadata);
    let blocker = blocker.or_else(|| {
        if !attempt.target_matches(current_target.as_ref()) {
            Some(InspectionBlocker::TargetChanged)
        } else if backup {
            Some(InspectionBlocker::BackupRetained)
        } else if held {
            Some(InspectionBlocker::OriginalRetained)
        } else if needs_payload && !payload {
            Some(InspectionBlocker::PayloadUnconfirmed)
        } else if needs_manifest && attempt.source_entries.is_none() {
            Some(InspectionBlocker::DirectorySnapshotMissing)
        } else {
            None
        }
    });
    let Some(blocker) = blocker else {
        if let Some(manifest) = &attempt.source_entries {
            manifest.verify(cancel, |_, _| {})?;
        }
        return Ok(InspectionReport {
            finding: InspectionFinding::Ready,
            paths,
        });
    };

    if attempt.copied
        && source.matches(current_source.as_ref())?
        && let (Some(left), Some(right)) = (&current_source, &current_target)
        && copied_contents_match(source, attempt, left, right, progress, cancel)?
    {
        return Ok(InspectionReport {
            finding: InspectionFinding::MatchingContents(kind),
            paths,
        });
    }

    if !attempt.copied
        && current_source.is_none()
        && current_target.is_some()
        && moved_tree_matches(source, attempt, cancel)?
    {
        return Ok(InspectionReport {
            finding: InspectionFinding::MatchingMove,
            paths,
        });
    }
    Ok(InspectionReport {
        finding: InspectionFinding::Unconfirmed(blocker),
        paths,
    })
}

fn observe_path(
    role: InspectionRole,
    location: Option<Location>,
    paths: &mut Vec<InspectedPath>,
) -> Result<bool, TransferError> {
    let Some(location) = location else {
        return Ok(false);
    };
    let present = entry_metadata(&location)?.is_some();
    paths.push(InspectedPath {
        role,
        location,
        present,
    });
    Ok(present)
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
            let parent = file
                .parent()
                .ok_or_else(|| TransferPlanError::MissingParent(file.clone()))?;
            let name = file
                .path()
                .file_name()
                .ok_or_else(|| TransferPlanError::MissingName(file.clone()))?;
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
    Ok(result && source.matches(entry_metadata(&source.resolved)?.as_ref())?)
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
            .map_err(|_| MoveHistoryError::Descendant {
                source: original.clone(),
                target: attempt.target.clone(),
                problem: DescendantProblem::Unmapped,
            })?;
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
