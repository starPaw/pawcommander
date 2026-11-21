mod baseline;
mod journal;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(in crate::vfs) use tests::two_way_round_trip;

use super::{
    apply::{self, Execution},
    check_new_name, observe_folders, revalidate_folders, Budget, Exclusions, Kind, ReadScope,
    Snapshot, SyncChange, SyncRow,
};
use crate::{
    i18n::text,
    operations::{Cancellation, Progress, TransferError},
    vfs::{transfer::validate_source_child_name, Location},
};
use baseline::BaselineStore;
use journal::Run;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

type Tree = BTreeMap<PathBuf, Kind>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReconcileConflict {
    FirstComparison,
    BothChanged,
    EditAndDelete,
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reconcile {
    Change {
        destination: usize,
        change: SyncChange,
    },
    Conflict(ReconcileConflict),
}

pub(crate) struct TwoWayRow {
    pub(crate) path: PathBuf,
    pub(crate) outcome: Reconcile,
}

pub(crate) struct TwoWayPlan {
    folders: [Location; 2],
    observed: [Snapshot; 2],
    exclusions: Exclusions,
    store: BaselineStore,
    baseline: Option<Tree>,
    matches_baseline: bool,
    interrupted: Option<Run>,
    choices: BTreeMap<PathBuf, usize>,
    pub(crate) rows: Vec<TwoWayRow>,
    pub(crate) conflicts: usize,
}

impl TwoWayPlan {
    pub(crate) fn build(
        folders: [Location; 2],
        exclusions: Exclusions,
        progress: &mut Progress,
    ) -> Result<Self, TransferError> {
        let data =
            dirs::data_local_dir().ok_or_else(|| text("Папка данных приложения недоступна"))?;
        Self::build_in(
            folders,
            data.join("PawCommander/sync-baselines"),
            exclusions,
            progress,
        )
    }

    fn build_in(
        folders: [Location; 2],
        directory: PathBuf,
        exclusions: Exclusions,
        progress: &mut Progress,
    ) -> Result<Self, TransferError> {
        let cancel = progress.cancellation();
        let (_, observed) = observe_folders(&folders, &exclusions, progress)?;
        let store = BaselineStore::new(directory, &observed, exclusions.patterns())?;
        let baseline = store.read(&cancel)?;
        let interrupted = store.read_run()?;
        if let Some(run) = &interrupted {
            store.check_stage(run, &observed)?;
        }
        let choices = BTreeMap::new();
        let (rows, conflicts) = reconcile(
            &observed,
            baseline.as_ref(),
            interrupted.is_some(),
            &choices,
            &cancel,
        )?;
        let matches_baseline = rows.is_empty()
            && baseline
                .as_ref()
                .is_some_and(|tree| matches_tree(&observed[0], tree));
        Ok(Self {
            folders,
            observed,
            exclusions,
            store,
            baseline,
            matches_baseline,
            interrupted,
            choices,
            rows,
            conflicts,
        })
    }

    pub(crate) fn has_baseline(&self) -> bool {
        self.baseline.is_some()
    }

    pub(crate) fn can_remember(&self) -> bool {
        self.rows.is_empty() && (!self.matches_baseline || self.interrupted.is_some())
    }

    pub(crate) fn interrupted(&self) -> bool {
        self.interrupted.is_some()
    }

    pub(crate) fn excluded_count(&self) -> usize {
        self.observed
            .iter()
            .map(|snapshot| snapshot.excluded.len())
            .sum()
    }

    pub(crate) fn can_apply(&self) -> bool {
        self.conflicts == 0 && !self.rows.is_empty()
    }

    pub(crate) fn folders(&self) -> &[Location; 2] {
        &self.folders
    }

    pub(crate) fn deletes_remote(&self) -> bool {
        self.folders.iter().any(Location::remote)
            && self.rows.iter().any(|row| {
                matches!(row.outcome, Reconcile::Change { destination, change: SyncChange::Delete }
                if self.folders[destination].remote())
            })
    }

    pub(crate) fn missing(&self, path: &Path, side: usize) -> bool {
        !self.observed[side].entries.contains_key(path)
    }

    pub(crate) fn choose(
        &mut self,
        path: PathBuf,
        side: usize,
        progress: &Progress,
    ) -> Result<(), TransferError> {
        if side > 1
            || !self
                .rows
                .iter()
                .any(|row| row.path == path && matches!(row.outcome, Reconcile::Conflict(_)))
        {
            return Err(text("Конфликт изменился. Постройте план заново.").into());
        }
        let mut choices = self.choices.clone();
        choices.insert(path, side);
        let (rows, conflicts) = reconcile(
            &self.observed,
            self.baseline.as_ref(),
            self.interrupted(),
            &choices,
            &progress.cancellation(),
        )?;
        self.choices = choices;
        self.rows = rows;
        self.conflicts = conflicts;
        Ok(())
    }

    fn revalidate(&self, progress: &mut Progress) -> Result<[ReadScope; 2], TransferError> {
        let cancel = progress.cancellation();
        let scopes = revalidate_folders(&self.folders, &self.observed, &self.exclusions, progress)?;
        self.store.check_roots(&self.observed)?;
        if self.store.read(&cancel)? != self.baseline || self.store.read_run()? != self.interrupted
        {
            return Err(text("Общее состояние изменилось. Постройте план заново.").into());
        }
        if let Some(run) = &self.interrupted {
            self.store.check_stage(run, &self.observed)?;
        }
        Ok(scopes)
    }

    pub(crate) fn verify(&self, progress: &mut Progress) -> Result<(), TransferError> {
        self.revalidate(progress).map(drop)
    }

    pub(crate) fn remember(&mut self, progress: &mut Progress) -> Result<(), TransferError> {
        if !self.can_remember() {
            return Err(text(
                "Чтобы сохранить общее состояние, содержимое папок должно совпадать.",
            )
            .into());
        }
        let cancel = progress.cancellation();
        let lock = self.store.lock()?;
        self.verify(progress)?;
        let common = material(&self.observed[0])
            .map(|(path, kind)| (path.clone(), kind.clone()))
            .collect();
        lock.save(&common, &cancel)?;
        if self.interrupted.is_some() {
            lock.finish_run()?;
        }
        self.baseline = Some(common);
        self.matches_baseline = true;
        self.interrupted = None;
        Ok(())
    }

    pub(crate) fn apply(&self, progress: &mut Progress) -> Result<String, String> {
        if !self.can_apply() {
            return Err(text("Для применения нужен план без нерешённых конфликтов.").into());
        }
        let rows: Vec<_> = self
            .rows
            .iter()
            .filter_map(|row| {
                let Reconcile::Change {
                    destination,
                    change,
                } = row.outcome
                else {
                    return None;
                };
                Some(SyncRow {
                    path: row.path.clone(),
                    destination,
                    change,
                })
            })
            .collect();
        let execution = Execution {
            folders: &self.folders,
            observed: &self.observed,
            rows: &rows,
            exclusions: &self.exclusions,
        };
        execution.plan_history(progress);
        let cancel = progress.cancellation();
        let mut completed = 0;
        let result = (|| {
            let lock = self.store.lock()?;
            let scopes = self.revalidate(progress)?;
            let target = self.target_tree(&rows, &cancel)?;
            let mut run = Run::new();
            lock.save_run(&run, &cancel)?;
            execution.apply_groups(
                &scopes,
                progress,
                &mut completed,
                Some(&mut |stage| lock.track_stage(&mut run, stage, &self.observed, &cancel)),
            )?;
            self.store.check_stage(&run, &self.observed)?;
            let (_, final_state) = observe_folders(&self.folders, &self.exclusions, progress)?;
            self.store.check_roots(&final_state)?;
            if !final_state.iter().all(|side| matches_tree(side, &target)) {
                return Err(text(
                    "После применения содержимое папок отличается от плана. Постройте план заново.",
                )
                .into());
            }
            lock.save(&target, &cancel)?;
            lock.finish_run()
        })();
        apply::finish(result, completed, progress)
    }

    fn target_tree(&self, rows: &[SyncRow], cancel: &Cancellation) -> Result<Tree, TransferError> {
        let mut target: Tree = material(&self.observed[0])
            .map(|(path, kind)| (path.clone(), kind.clone()))
            .collect();
        for row in rows.iter().filter(|row| row.destination == 0) {
            cancel.check()?;
            if row.change == SyncChange::Delete {
                target.remove(&row.path);
            } else if let Some(kind) = kind(&self.observed[1], &row.path) {
                target.insert(row.path.clone(), kind.clone());
            }
        }
        let mut budget = Budget::default();
        for (path, kind) in &target {
            cancel.check()?;
            let extra = match kind {
                Kind::Link(path) => path.len(),
                _ => 0,
            };
            // Both copies must fit inside the budget for the final pair scan.
            let bytes = path.as_os_str().len() + extra;
            budget.visit(bytes)?;
            budget.visit(bytes)?;
        }
        Ok(target)
    }
}

fn material(snapshot: &Snapshot) -> impl Iterator<Item = (&PathBuf, &Kind)> {
    snapshot
        .entries
        .iter()
        .filter(|(path, _)| !path.as_os_str().is_empty())
        .map(|(path, entry)| (path, &entry.kind))
}

fn matches_tree(snapshot: &Snapshot, tree: &Tree) -> bool {
    material(snapshot).eq(tree.iter())
}

fn reconcile(
    observed: &[Snapshot; 2],
    baseline: Option<&Tree>,
    interrupted: bool,
    choices: &BTreeMap<PathBuf, usize>,
    cancel: &Cancellation,
) -> Result<(Vec<TwoWayRow>, usize), TransferError> {
    let [left, right] = observed;
    let union: BTreeSet<_> = left
        .entries
        .keys()
        .chain(right.entries.keys())
        .chain(baseline.into_iter().flat_map(|tree| tree.keys()))
        .collect();
    let mut paths = union
        .into_iter()
        .filter(|path| !path.as_os_str().is_empty())
        .peekable();
    let mut rows = Vec::new();
    let mut conflicts = 0;
    let mut new_names = [BTreeSet::new(), BTreeSet::new()];
    while let Some(path) = paths.next() {
        cancel.check()?;
        let current = [kind(left, path), kind(right, path)];
        if current[0] == current[1] {
            // Common directories are traversed independently, allowing new
            // files under the same directory to merge in both directions.
            continue;
        }
        let decision = match choices.get(path) {
            Some(keep) => Ok(1 - keep),
            None if interrupted => Err(ReconcileConflict::Interrupted),
            None => decide(observed, current, baseline, path),
        };
        let branch = std::iter::once(path).chain(std::iter::from_fn(|| {
            paths.next_if(|next| next.starts_with(path))
        }));
        match decision {
            Ok(destination) => {
                let source = 1 - destination;
                for path in branch {
                    cancel.check()?;
                    let from = kind(&observed[source], path);
                    let to = kind(&observed[destination], path);
                    let change = match (from, to) {
                        (Some(_), None) => {
                            check_new_name(
                                &observed[destination],
                                path,
                                &mut new_names[destination],
                            )?;
                            SyncChange::Copy
                        }
                        (None, Some(_)) => SyncChange::Delete,
                        (Some(from), Some(to)) if from != to => SyncChange::Replace,
                        _ => continue,
                    };
                    if change != SyncChange::Copy {
                        observed[destination].protect_excluded(path)?;
                    }
                    if left.root.remote() || right.root.remote() {
                        for name in path.iter() {
                            validate_source_child_name(&observed[source].root, name)?;
                        }
                    }
                    rows.push(TwoWayRow {
                        path: path.clone(),
                        outcome: Reconcile::Change {
                            destination,
                            change,
                        },
                    });
                }
            }
            Err(reason) => {
                conflicts += 1;
                rows.push(TwoWayRow {
                    path: path.clone(),
                    outcome: Reconcile::Conflict(reason),
                });
                // A structural conflict owns the branch. Its descendants
                // must not yield automatic writes against the disputed root.
                for _ in branch {
                    cancel.check()?;
                }
            }
        }
    }
    Ok((rows, conflicts))
}

fn kind<'a>(snapshot: &'a Snapshot, path: &Path) -> Option<&'a Kind> {
    snapshot.entries.get(path).map(|entry| &entry.kind)
}

fn decide(
    observed: &[Snapshot; 2],
    current: [Option<&Kind>; 2],
    baseline: Option<&Tree>,
    path: &Path,
) -> Result<usize, ReconcileConflict> {
    let Some(baseline) = baseline else {
        return match current {
            [None, Some(_)] => Ok(0),
            [Some(_), None] => Ok(1),
            _ => Err(ReconcileConflict::FirstComparison),
        };
    };
    let matches_previous = |snapshot: &Snapshot| {
        snapshot
            .subtree(path)
            .map(|(path, entry)| (path, &entry.kind))
            .eq(baseline
                .range(path.to_owned()..)
                .take_while(|(candidate, _)| candidate.starts_with(path)))
    };
    match [
        matches_previous(&observed[0]),
        matches_previous(&observed[1]),
    ] {
        [true, false] => Ok(0),
        [false, true] => Ok(1),
        _ if current.contains(&None) => Err(ReconcileConflict::EditAndDelete),
        _ => Err(ReconcileConflict::BothChanged),
    }
}
